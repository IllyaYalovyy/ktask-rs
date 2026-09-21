//! What a repeated failure looks like, and the breaker that stops counting it.
//!
//! VISION.md §7 asks for two things of self-healing that bound it: repeated
//! identical failure signatures are *detected*, and a circuit breaker then
//! trips. Both need one thing no other module produces: a name for a failure
//! that survives being seen twice. A gate result is the wrong shape for that —
//! it carries the seconds the command ran, the checkout it ran in and the line
//! number a panic landed on, and all three move between two runs of the very
//! same broken test. [`signature`] removes them and hashes what is left, so
//! "the same failure again" is a comparison of two short strings rather than a
//! judgement about a transcript.
//!
//! # What the signature is over
//!
//! The failure class, and the failing test names the refusing gates reported.
//! Names come from [`crate::parse_cargo`] — the same reading that gives the
//! TUI's failures screen its list — so the signature and the screen agree about
//! which tests refused. A gate that refused without naming any test (a build
//! that never compiled, a lint that never ran one) contributes its own kind
//! beside the first line of what it did say, which keeps two different
//! refusals apart instead of collapsing every nameless failure into one
//! signature that would trip the breaker on unrelated work.
//!
//! Names are then normalized: digits, paths and timings go, because those are
//! the three things a rerun changes while the failure stays. `case_1` and
//! `case_12` are one parametrised test failing; `/home/alice/wt/…` and
//! `/home/ci/agent-7/checkout/…` are one file in two checkouts; `took 3s` and
//! `took 4200ms` are one refusal that waited longer. The set of names is sorted
//! and deduplicated before it is hashed, so a run that reported its two
//! failures in the other order, or one test that two test binaries both
//! reported, is the failure it already was.
//!
//! The cost of stripping digits is that two tests whose names differ *only* by
//! a digit are one signature. That is accepted deliberately: a test named
//! `case_1` and one named `case_2` are one body of code with two inputs, and
//! the answer VISION.md §7 wants from a breaker is the same for both.
//!
//! # What a trip is worth
//!
//! [`Breaker`] counts a signature, not a run: the threshold in the project's
//! configuration is a count of *identical* failures, and a failure that
//! alternates with a different one is still the same failure returning. Once a
//! signature has reached the threshold the breaker is tripped and stays
//! tripped — a breaker that closes again because the next failure was
//! worded differently is a counter, not a breaker. [`trip_event`] is the
//! journal record a trip leaves behind, so the run that stopped says so in the
//! same append-only file every other decision is in.

//! # What bounds the attempts
//!
//! §7 bounds self-healing three ways at once — attempts, wall-clock time and
//! tokens — and a breaker counts none of them. [`Breaker`] answers the
//! failure-shaped half of that bound; [`should_continue`] holds the arithmetic
//! half, because the two catch different loops: a remediation that repeats
//! itself trips a breaker, and one that fails differently every time only ever
//! stops on a figure. Given how many attempts have been refused, what the
//! remediation has cost and what it has spent, it returns the bound that says
//! stop and names it, so a stopped run reads back as *why* it stopped rather
//! than merely that it did.
//!
//! # What a bundle is
//!
//! VISION.md §7 requires that every remediation launch a *fresh* provider
//! session seeded with a compact failure bundle: the classification, the gate
//! output, the diff summary and the prior attempts' evidence. [`bundle`] is that
//! bundle. It is a projection of the evidence handed to it, because a bundle
//! cannot read a git tree or open a journal from inside a formatter: the diff and
//! the attempt records are the runner's to hand over.
//!
//! # Why the same failure is the same bytes
//!
//! A bundle holds *what happened*, not *when*. A stopwatch reading, a wall-clock
//! instant and a session id are the three things a rerun of one failure always
//! changes, and a bundle that carried them would not match itself across two
//! runs — which is the property VISION.md §7's determinism and T069's done-when
//! both ask for. The instants stay in the journal, where a run's timing is read
//! from; what earns a place in a budget is the refusal, the diff, and what the
//! earlier attempts made of the same task. Prior attempts are sorted by their own
//! number for the same reason [`crate::attempt_records`] sorts: evidence is filed
//! when an attempt's recorder reaches it, and one failure handed over in another
//! order is one bundle, not two.
//!
//! # Where the budget goes
//!
//! `budget_bytes` is a ceiling in bytes and the bundle never costs more of them.
//! What is lost is decided by one rule: *write what a session needs most last,
//! and shed from the front*. The blocks are laid down oldest first — the oldest
//! prior attempt, then the newer ones, then this attempt's diff summary, then the
//! gates that refused in the order they ran, then the frame that names the
//! class — and trimmed from the front of that order, so a block that only partly
//! fits keeps its tail. Read back, the bundle is newest-first: the failure being
//! remediated at the top and the history beneath it, and a budget too small for
//! the frame is left holding the class line, which is the one line that decides
//! what the response to a failure is (VISION.md §7).
//!
//! Keeping tails is also why a gate contributes the *tail* of what it wrote: the
//! last lines of a refusing command are its verdict and the first are its
//! progress. And the frame counts what it holds — how many prior attempts there
//! were, and which kinds of gate refused — so a bundle trimmed below that
//! evidence still says how much there was, rather than letting a session conclude
//! that there was less.
//!
//! # Redaction, before the cut
//!
//! Every field goes through [`crate::redact::redact`] *before* it is trimmed, and
//! that order is not interchangeable. A cut can land inside a secret, and half a
//! shaped secret is a shape the redaction table no longer recognises: truncating
//! first and redacting second leaks exactly as much as it must. Redacting first
//! means the worst a cut can do is halve a mask, which leaks nothing.
//!
//! The signature T069 fixes carries no `secret_patterns`, so a bundle honours the
//! built-in table alone. A project that has named the shape its own keys take
//! redacts them where the bundle's inputs were read; that gap is reported rather
//! than quietly closed here, because closing it would mean widening a signature
//! another task is already written against.
//!
//! # What an attempt may not touch
//!
//! VISION.md §3's invariant 5 and §2's "no self-modification of policy" say one
//! thing from two directions: a recovery cannot weaken the rules it is judged
//! by, and neither can the attempt that came before it. A prompt asking the
//! agent not to edit `clippy.toml` is a request. [`check_no_policy_edit`] is the
//! check — run against the paths `git` says changed, never against the diff the
//! agent describes, and asked of **every** attempt rather than only of a
//! remediation. That is not a quirk of the wording: the attempt that exists
//! because a gate refused is the attempt most motivated to edit that gate, but
//! the first attempt that edits the lint configuration has broken the identical
//! rule, and an attempt that had to edit `scripts/quality.sh` to get past it
//! would have proven nothing by getting past it. The signature carries no
//! attempt number, which is what makes the rule unable to care about one.
//!
//! [`policy_edit_event`] is the journal record the refusal leaves, so a run that
//! stopped on a protected path says so — with the paths — in the same
//! append-only file every other decision is in.

use regex::Regex;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use time::Duration;

use crate::{
    AttemptRecord, Error, EventKind, FailureClass, GateResult, Result, Task, Usage, parse_cargo,
    redact::redact,
};

/// How many hexadecimal characters a signature carries.
///
/// Sixty-four bits is far past what comparing the handful of signatures one task
/// produces needs, and a signature is read by a human on the failures screen as
/// well as compared by a breaker — the project id is truncated for the same
/// reason (ADR-0003).
const SIGNATURE_HEX_CHARS: usize = 16;

/// One normalization step: a shape to remove and what a match of it becomes.
///
/// The shape is compiled the first time it is read, the way every other phrase
/// table in this crate is (ADR-0059), and a shape that does not compile is
/// skipped rather than panicking — a supervisor that panics loses the run. That
/// is only honest because `every_normalization_rule_compiles` insists the four
/// below do: a silently missing rule would make two runs of one failure look
/// like two failures, which spends a remediation budget on a test nobody
/// changed.
struct Rule {
    /// The shape as it is written in this file.
    source: &'static str,
    /// What a match of it becomes.
    replacement: &'static str,
    /// The same shape compiled, on first use.
    compiled: OnceLock<Option<Regex>>,
}

impl Rule {
    /// A shape that is not compiled yet.
    const fn new(source: &'static str, replacement: &'static str) -> Self {
        Self {
            source,
            replacement,
            compiled: OnceLock::new(),
        }
    }

    /// The compiled shape, or `None` when the shape is not a regular expression.
    fn regex(&self) -> Option<&Regex> {
        self.compiled
            .get_or_init(|| Regex::new(self.source).ok())
            .as_ref()
    }

    /// This shape applied to `text`, which is `text` itself when the shape
    /// never compiled.
    fn apply(&self, text: &str) -> String {
        match self.regex() {
            Some(pattern) => pattern.replace_all(text, self.replacement).into_owned(),
            None => text.to_owned(),
        }
    }
}

/// A number beside the unit that says how long something took.
///
/// The units are ordered longest first so `250ms` is a quarter of a second and
/// not `250m` with an `s` left over. The unit goes with the number because a
/// rule that removed only the digits would leave `took s` beside `took ms` and
/// call one refusal two.
static TIMINGS: Rule = Rule::new(
    concat!(
        r"(?i)\b\d+(?:[.,]\d+)?[ \t]*(milliseconds?|ms|microseconds?|us|nanoseconds?|ns|",
        r"weeks?|wks?|w|days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|sec|s)\b",
    ),
    "",
);

/// A directory, with the separator that closes it.
///
/// Only a segment that *ends* in a separator goes, so the file a message names
/// survives and the checkout it lives in does not: the same broken file in
/// `/home/alice/wt/` and in `/home/ci/agent-7/checkout/` is one failure, and
/// the directory a run happened to be in is not part of what failed.
static PATHS: Rule = Rule::new(r"[\w.\-]+[/\\]+", "");

/// Any run of digits, which is where line numbers, counters, byte counts and
/// parametrised case numbers all live.
static DIGITS: Rule = Rule::new(r"\d+", "");

/// Punctuation and whitespace run together, which is what stripping leaves
/// behind: `gate.rs:41:5:` becomes `gate.rs` and `a::b` becomes `a b`.
///
/// Underscores are deliberately absent. They are what joins the words of a
/// Rust test name, and removing them would smear every name into the same run
/// of letters.
static SEPARATORS: Rule = Rule::new(r"[\s:/\\.]+", " ");

/// The four rules, in the order they are applied.
///
/// Held by reference because a `Rule` compiles itself once and so is shared,
/// never copied.
static RULES: [&Rule; 4] = [&TIMINGS, &PATHS, &DIGITS, &SEPARATORS];

/// Name a failure so a later one can be recognized as it.
///
/// The class is hashed *with* the names rather than beside them: the class is
/// what decides the response (VISION.md §7), so one test that fails once as a
/// [`FailureClass::VerificationFailure`] and once as a
/// [`FailureClass::EnvironmentFailure`] is two failures, and counting them
/// together would spend a breaker on a machine that changed underneath the run.
///
/// `gates` is the whole set a run recorded, passing gates included: only the ones
/// that refused contribute, so one broken test keeps one signature whether or not
/// the lint gate happened to run first.
#[must_use]
pub fn signature(class: FailureClass, gates: &[GateResult]) -> String {
    let mut names = BTreeSet::new();
    for gate in gates.iter().filter(|gate| !gate.passed) {
        contribute(gate, &mut names);
    }
    // The class name is the spelling the journal writes the class with in its
    // own payload, which `classify`'s vocabulary test pins, so the two agree
    // without a second table of words to keep in step.
    let mut key = format!("{class:?}");
    for name in names {
        key.push('\n');
        key.push_str(&name);
    }
    hashed(&key)
}

/// Add one refusing gate's failure names to `names`.
///
/// A gate that named its failures contributes every one of them; a gate that
/// refused without naming any contributes its kind beside the first line it did
/// write, which is what keeps two unrelated nameless refusals apart.
fn contribute(gate: &GateResult, names: &mut BTreeSet<String>) {
    let reported = reported_names(gate);
    if reported.is_empty() {
        names.insert(silent_name(gate));
    } else {
        names.extend(reported);
    }
}

/// The tests a refusing gate reported by name, normalized.
///
/// The reading is [`parse_cargo`], the same one the failures screen lists, so
/// the signature and the screen cannot disagree about which tests refused.
/// Cargo writes both of its streams and a transcript arrives with the test names
/// in either, so the two are read as the one transcript they are.
fn reported_names(gate: &GateResult) -> Vec<String> {
    let transcript = format!("{}\n{}", gate.stdout, gate.stderr);
    parse_cargo(&transcript)
        .map(|summary| summary.failures)
        .unwrap_or_default()
        .iter()
        .map(|name| normalize(name))
        .filter(|name| !name.is_empty())
        .collect()
}

/// What a gate that named no test is known by: its kind, and the first line it
/// said anything on.
///
/// Stderr is read first because that is where a build or a lint writes its
/// verdict, and the first line is the first thing a repeated refusal repeats.
/// A gate that refused in silence is known by its kind alone.
fn silent_name(gate: &GateResult) -> String {
    let verdict = gate
        .stderr
        .lines()
        .chain(gate.stdout.lines())
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    normalize(&format!("{} {verdict}", gate.kind.as_str()))
}

/// Strip the three things a rerun changes while the failure stays: timings,
/// paths and digits, then collapse what the stripping left into single spaces.
fn normalize(text: &str) -> String {
    let mut text = text.to_owned();
    for rule in RULES {
        text = rule.apply(&text);
    }
    text.trim().to_owned()
}

/// What recording one more failure did to the breaker's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerState {
    /// This signature has now been recorded `seen` times, which is short of the
    /// threshold: another remediation is still paid for.
    Allowed {
        /// How many times this signature has been recorded.
        seen: u32,
    },
    /// The budget is spent. `signature` is the one that spent it and `seen` is
    /// how many times it arrived — the signature is repeated here because a
    /// breaker that is already tripped reports the trip it holds, not the
    /// failure that happened to arrive afterwards.
    Tripped {
        /// The signature whose repeats spent the budget.
        signature: String,
        /// How many records of it the threshold cost.
        seen: u32,
    },
}

/// Counts identical failure signatures until the configured threshold is spent.
///
/// A breaker belongs to one task's remediation, which is the caller's to
/// create: it is the object that answers "have we seen this before", and VISION.md
/// §7's bound is on identical failures, not on attempts — a failure that
/// alternates with a different one is still the same failure returning, and a
/// counter that forgot it whenever something else went wrong never trips at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breaker {
    /// How many records of one signature trip it.
    threshold: u32,
    /// How many times each signature has been recorded.
    seen: BTreeMap<String, u32>,
    /// The trip, once there has been one: a breaker does not close again.
    tripped: Option<(String, u32)>,
}

impl Breaker {
    /// A breaker that trips once one signature has been recorded `threshold`
    /// times — the project's `circuit_breaker_threshold`, which [`crate::Config`]
    /// defaults to three. A threshold of zero or one trips on the first record.
    #[must_use]
    pub const fn new(threshold: u32) -> Self {
        Self {
            threshold,
            seen: BTreeMap::new(),
            tripped: None,
        }
    }

    /// Record one more occurrence of `sig` and say what the budget looks like
    /// now.
    ///
    /// A trip is sticky. A run that tripped and then failed for a slightly
    /// different reason has not been fixed, and handing it another session
    /// because the next transcript was worded differently is how a bounded
    /// remediation becomes an unbounded one.
    pub fn record(&mut self, sig: &str) -> BreakerState {
        if let Some((signature, seen)) = &self.tripped {
            return BreakerState::Tripped {
                signature: signature.clone(),
                seen: *seen,
            };
        }
        let seen = self.seen.entry(sig.to_owned()).or_insert(0);
        *seen = seen.saturating_add(1);
        if *seen >= self.threshold {
            self.tripped = Some((sig.to_owned(), *seen));
            BreakerState::Tripped {
                signature: sig.to_owned(),
                seen: *seen,
            }
        } else {
            BreakerState::Allowed { seen: *seen }
        }
    }
}

/// The journal record a trip leaves behind.
///
/// A tripped breaker ends the task rather than launching another remediation,
/// and `docs/DESIGN.md` already has an entry for exactly that: `TaskFailed`,
/// whose `class` is the classified cause and whose `detail` says what ended it.
/// The catalog admits no entry without the `state::apply` arm that answers it,
/// and this one needs none — the machine already fails a task from `Running` and
/// from `Remediating` when a `TaskFailed` arrives (ADR-0022).
#[must_use]
pub fn trip_event(class: FailureClass, signature: &str, seen: u32) -> EventKind {
    EventKind::TaskFailed {
        class,
        detail: format!(
            "circuit breaker tripped: signature {signature} has now failed {seen} times, and \
             remediation is spent"
        ),
    }
}

/// The two spaces every line of a gate's or a diff's own text sits under the
/// line that names it.
const INDENT: &str = "  ";

/// One part of a bundle — the frame, one diff summary, one refusing gate, one
/// prior attempt — as the lines it contributes, oldest first.
type Block = Vec<String>;

/// The compact failure bundle a fresh remediation session is launched with.
///
/// VISION.md §7 requires that a remediation is a *new* session seeded with a
/// compact bundle rather than a continuation of the session that failed, and
/// that the bundle carries the classification, the gate output, the diff summary
/// and the prior attempts' outcomes. [`bundle`] is that bundle. It is a
/// projection of the evidence handed to it rather than a reader of it: the diff
/// and the attempt records are the runner's to gather, and a function that
/// neither reads a git tree nor opens a journal is one whose output a rerun can
/// be expected to reproduce byte for byte.
///
/// # Shape
///
/// The evidence is laid down oldest first — the prior attempts in attempt order,
/// then this attempt's diff summary, then the gates that refused in the order
/// they ran, then the frame that names the task and the class — and rendered
/// newest-first, so a session reads the refusal it is being asked to fix before
/// the history underneath it. Refusing gates contribute the whole of what they
/// wrote; satisfied ones contribute nothing, because a green gate's chatter is
/// not evidence and spends the budget a refusal needs.
///
/// # Where the budget goes
///
/// `budget_bytes` is a ceiling in bytes and the bundle never costs more of them;
/// `0` answers with an empty bundle. What does not fit is shed from the front of
/// the oldest-first order, so a block that only partly fits keeps its *tail* —
/// the verdict at the end of a refusing command rather than its progress, and
/// the newest attempt rather than the oldest. The frame is laid down last and so
/// is shed last, which is why a bundle trimmed to a handful of bytes is still a
/// classified failure; it also counts the evidence a tighter budget dropped, so
/// a session cannot conclude that there was less of it than there was.
///
/// # Redaction, before the cut
///
/// Every piece of free text goes through [`crate::redact::redact`] *before* any
/// of it is trimmed, never after. A cut can land inside a secret, and half a
/// shaped secret is a shape the table of shapes (ADR-0032) no longer recognises,
/// so redacting after trimming leaks exactly as much as it must avoid. Redacting
/// first means the worst a cut can do is halve a [`crate::redact::MASK`], which
/// leaks nothing.
///
/// [`crate::redact::redact`] is called with no configured patterns, because the
/// signature this function is specified by carries none: a project's own
/// `secret_patterns` are honoured where a bundle's inputs are read, and
/// widening this signature to reach them would change what every other caller
/// compares.
///
/// # What is deliberately absent
///
/// No instant, no stopwatch reading, no session or model id, no base sha. Those
/// are the things a rerun of one failure always changes, and a bundle carrying
/// them would not match itself across two runs — which is the determinism
/// VISION.md §7 ranks above token economy, and what the signature and the
/// breaker compare. They stay in the journal, which is where a run's timing is
/// read from ([`crate::attempt_records`]).
#[must_use]
pub fn bundle(
    task: &Task,
    class: FailureClass,
    gates: &[GateResult],
    diff_summary: &str,
    prior: &[AttemptRecord],
    budget_bytes: usize,
) -> String {
    let mut blocks = oldest_first(prior);
    blocks.push(diff_block(diff_summary));
    blocks.extend(gate_blocks(gates));
    blocks.push(frame(task, class, gates, prior));
    let mut draft = Draft::new(blocks);
    draft.fit(budget_bytes);
    draft.render()
}

/// A bundle's blocks while they are being fitted to a budget.
struct Draft {
    /// The blocks, oldest first: the front of this list is what is shed first.
    blocks: Vec<Block>,
    /// The bytes [`Draft::render`] would cost for these blocks as they stand.
    bytes: usize,
}

impl Draft {
    /// The draft those blocks cost, before any trimming.
    fn new(blocks: Vec<Block>) -> Self {
        let text: usize = blocks.iter().flatten().map(String::len).sum();
        let separators = blocks.iter().map(Vec::len).sum::<usize>().saturating_sub(1);
        Self {
            blocks,
            bytes: text.saturating_add(separators),
        }
    }

    /// Shed from the front of the oldest block until the draft fits `budget`.
    ///
    /// The cut is a byte cut at a character boundary, and it always makes
    /// progress: a line whose first character is too wide for what remains of
    /// the budget goes entirely rather than the bundle going over it.
    fn fit(&mut self, budget: usize) {
        while self.bytes > budget {
            let deficit = self.bytes.saturating_sub(budget);
            let Some(line) = self.blocks.first_mut().and_then(|block| block.first_mut()) else {
                self.bytes = 0;
                return;
            };
            let shed = front_of(line, deficit);
            line.drain(..shed);
            self.bytes = self.bytes.saturating_sub(shed);
            let emptied = line.is_empty();
            if emptied {
                self.drop_front_line();
            }
        }
    }

    /// Discard the emptied line at the front, and the separator it took with it.
    ///
    /// A block whose only line has gone is dropped whole, so the block label is
    /// what a trimmed transcript loses first — which is why [`frame`] counts the
    /// evidence rather than only showing it.
    fn drop_front_line(&mut self) {
        if self.blocks.first().is_some_and(|block| block.len() <= 1) {
            self.blocks.remove(0);
        } else if let Some(block) = self.blocks.first_mut() {
            block.remove(0);
        }
        if self.blocks.is_empty() {
            self.bytes = 0;
        } else {
            self.bytes = self.bytes.saturating_sub(1);
        }
    }

    /// The blocks as the bundle: newest block first, oldest last, no trailing
    /// newline to spend the budget on.
    fn render(&self) -> String {
        let lines: Vec<&str> = self
            .blocks
            .iter()
            .rev()
            .flatten()
            .map(String::as_str)
            .collect();
        lines.join("\n")
    }
}

/// How many bytes come off the front of `line` to give back `deficit` of them.
///
/// A cut in the middle of a character is not a cut at all — it is an invalid
/// `String` — so the cut backs off to the nearest boundary it can reach. Only
/// when the whole of the deficit is smaller than the first character does it
/// overshoot, and by less than one character.
fn front_of(line: &str, deficit: usize) -> usize {
    let want = deficit.min(line.len());
    if line.is_char_boundary(want) {
        return want;
    }
    let mut boundary = want;
    while boundary > 0 && !line.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    if boundary > 0 {
        return boundary;
    }
    line.chars().next().map_or(0, char::len_utf8)
}

/// The four lines that name the failure, laid down last so they are shed last.
///
/// The counts are why this is a frame and not a heading. A bundle trimmed below
/// its own evidence says how much evidence there was, so a session that was
/// handed a third of it cannot conclude that there was a third.
fn frame(task: &Task, class: FailureClass, gates: &[GateResult], prior: &[AttemptRecord]) -> Block {
    let id = task.id;
    let title = scrub(task.title());
    let refused = list_or(&refused_kinds(gates), "none");
    let attempts = prior.len();
    vec![
        format!("task {id}: {title}"),
        format!("prior attempts: {attempts}"),
        format!("gates refused: {refused}"),
        format!("class: {class:?}"),
    ]
}

/// The kinds of the gates that refused, in the order they ran.
fn refused_kinds(gates: &[GateResult]) -> Vec<&'static str> {
    gates
        .iter()
        .filter(|gate| !gate.passed)
        .map(|gate| gate.kind.as_str())
        .collect()
}

/// One line per prior attempt, in attempt order.
///
/// Sorted by the attempt's own number rather than left as they were handed over,
/// because [`crate::attempt_records`] files evidence as each attempt's recorder
/// reaches it: one failure gathered in another order is one bundle, not two.
fn oldest_first(prior: &[AttemptRecord]) -> Vec<Block> {
    let mut ordered: Vec<&AttemptRecord> = prior.iter().collect();
    ordered.sort_by_key(|record| record.id);
    ordered
        .iter()
        .map(|record| vec![attempt_line(record)])
        .collect()
}

/// What one earlier attempt did, on the one line a bounded bundle affords it.
///
/// The gates it refused, the commit it produced, what it spent and the sentence
/// it stopped with are the four facts that stop a remediation re-running a fix
/// that already failed; its transcript, its session id and its clock are not.
fn attempt_line(attempt: &AttemptRecord) -> String {
    let refused = list_or(&refused_kinds(&attempt.gates), "nothing");
    let commit = match &attempt.candidate_sha {
        Some(sha) => {
            let sha = scrub(sha);
            format!("produced {sha}")
        }
        None => "produced no commit".to_owned(),
    };
    let spent = spent(attempt.usage.as_ref());
    let stopped = sentence(&attempt.exit_reason);
    let number = attempt.id;
    format!("prior attempt {number}: refused {refused} | {commit} | {spent} | exited \"{stopped}\"")
}

/// What an attempt's session reported spending, or why nobody knows.
///
/// A figure that was never reported is left out rather than written as a zero.
/// [`crate::Usage::unavailable`] is the difference between a session that asked
/// and was told nothing and one that spent nothing, and a remediation that reads
/// `in=0` has been told the second of those.
fn spent(usage: Option<&Usage>) -> String {
    let Some(usage) = usage else {
        return "usage unasked".to_owned();
    };
    let mut figures = Vec::new();
    if let Some(input) = usage.input_tokens {
        figures.push(format!("in={input}"));
    }
    if let Some(output) = usage.output_tokens {
        figures.push(format!("out={output}"));
    }
    if let Some(cached) = usage.cached_tokens {
        figures.push(format!("cached={cached}"));
    }
    if let Some(cost) = usage.cost_usd {
        figures.push(format!("cost=${cost}"));
    }
    if figures.is_empty() {
        return "usage unreported".to_owned();
    }
    format!("usage {}", figures.join(" "))
}

/// This attempt's diff summary, under a label of its own.
fn diff_block(diff_summary: &str) -> Block {
    let mut block = vec![String::from("diff:")];
    let lines = body_lines(diff_summary);
    if lines.is_empty() {
        block.push(format!("{INDENT}no diff summary was given"));
    } else {
        block.extend(lines);
    }
    block
}

/// One block per refusing gate, in the order the gates ran.
fn gate_blocks(gates: &[GateResult]) -> Vec<Block> {
    gates
        .iter()
        .filter(|gate| !gate.passed)
        .map(gate_block)
        .collect()
}

/// A refusing gate: how it stopped, then the whole of what it said.
fn gate_block(gate: &GateResult) -> Block {
    let verdict = verdict(gate);
    let kind = gate.kind.as_str();
    let mut block = vec![format!("failing gate {kind} ({verdict}):")];
    let written = format!("{}\n{}", gate.stdout, gate.stderr);
    let lines = body_lines(&written);
    if lines.is_empty() {
        block.push(format!("{INDENT}the gate refused without writing anything"));
    } else {
        block.extend(lines);
    }
    block
}

/// How a gate stopped, in the one form that says why it is a failure.
///
/// A timeout is its own fact and the kill that enforced it is not the failure
/// ([`GateResult::timed_out`]); a process stopped by a signal has no exit code
/// to name, and a gate that reported neither has neither.
fn verdict(gate: &GateResult) -> String {
    if gate.timed_out {
        return "timed out".to_owned();
    }
    if let Some(signal) = gate.signal {
        return format!("signal {signal}");
    }
    if let Some(code) = gate.exit_code {
        return format!("exit {code}");
    }
    String::from("stopped without a verdict")
}

/// A piece of evidence's own text as redacted, indented lines.
///
/// Blank lines and trailing whitespace go: a bundle is read under a byte
/// ceiling, and the blank lines of a transcript are the part of it that carries
/// nothing.
fn body_lines(text: &str) -> Vec<String> {
    let scrubbed = scrub(text);
    scrubbed
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(|line| format!("{INDENT}{line}"))
        .collect()
}

/// Redact a piece of free text with the built-in table, before it is trimmed.
fn scrub(text: &str) -> String {
    redact(text, &[])
}

/// An exit reason as one line: redacted, and every run of whitespace folded into
/// a single space.
///
/// A prior attempt gets one line of a bounded bundle, and the reason an agent
/// stopped with is free text that is often several lines long.
fn sentence(text: &str) -> String {
    scrub(text)
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

/// The refusing gates' kinds, space-separated, or what to say when none refused.
///
/// The two empty answers differ because the two empties are different facts: a
/// run whose gates all refused nothing is a run that failed before the gates
/// could say anything about it.
fn list_or(items: &[&str], when_empty: &str) -> String {
    if items.is_empty() {
        return when_empty.to_owned();
    }
    items.join(" ")
}

/// The lowercase hexadecimal prefix of the SHA-256 of `text`.
fn hashed(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(SIGNATURE_HEX_CHARS / 2)
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect()
}

/// What one task's remediation is allowed to spend before it stops.
///
/// VISION.md §7 bounds self-healing with three figures — attempts, wall-clock
/// time and tokens — and [`Breaker`] counts none of them: it counts failure
/// signatures. The two bounds are needed because they catch different loops. A
/// remediation that keeps producing the *same* failure trips a breaker; one
/// that invents a new failure every attempt never reaches a threshold at all,
/// so nothing but a figure would ever stop it. The three are independent, so
/// each is checked on its own and each names itself when it stops the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Attempts that have already been tried and refused. Each remediation
    /// attempt launches a fresh provider session, so this is the ceiling on how
    /// many of those launches one task's failure may still pay for.
    pub max_attempts: u32,
    /// Wall-clock time the remediation has cost so far. It bounds elapsed time
    /// rather than the attempt count because one attempt can hang: a ceiling on
    /// attempts alone still has to wait out every session that never answers.
    pub max_elapsed: Duration,
    /// Tokens the remediation has spent, or [`None`] when nothing bounds them.
    /// The bound is optional because a token figure is a provider's answer, not
    /// a fact about the work: every [`crate::Usage`] field is an `Option` for
    /// the providers that report nothing, and a bound that cannot be measured
    /// must not stop anything.
    pub max_tokens: Option<u64>,
}

/// The bound that stopped a remediation, carrying the figures that spent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// `attempts` attempts were already refused against a ceiling of `max`.
    Attempts {
        /// Attempts the remediation had already made.
        attempts: u32,
        /// The ceiling [`Bounds::max_attempts`] set.
        max: u32,
    },
    /// `elapsed` seconds had passed against a ceiling of `max` seconds.
    Elapsed {
        /// Seconds the remediation had already cost. Whole seconds, because
        /// [`time::Duration`] is signed and a bound read as a count of seconds
        /// must be able to say so rather than refuse the figure.
        elapsed: i64,
        /// The ceiling [`Bounds::max_elapsed`] set, in whole seconds.
        max: i64,
    },
    /// `tokens` had been spent against a ceiling of `max`.
    Tokens {
        /// Tokens the remediation had already spent.
        tokens: u64,
        /// The ceiling [`Bounds::max_tokens`] set.
        max: u64,
    },
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Attempts { attempts, max } => {
                write!(f, "attempts {attempts} past the {max} bound")
            }
            Self::Elapsed { elapsed, max } => {
                write!(f, "elapsed {elapsed}s past the {max}s bound")
            }
            Self::Tokens { tokens, max } => write!(f, "tokens {tokens} past the {max} bound"),
        }
    }
}

/// What [`should_continue`] decided: another session, or the bound that ended it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No bound is spent, so the remediation may launch another session.
    Continue,
    /// A bound is spent, and [`Bound`] says which one spent it.
    Stop(Bound),
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Continue => f.write_str("no remediation bound is spent"),
            Self::Stop(bound) => bound.fmt(f),
        }
    }
}

/// Whether a remediation may launch another attempt, and which bound says not.
///
/// `attempts` counts the attempts already tried and refused, `elapsed` is what
/// the remediation has cost so far, and `tokens` what it has spent — a figure
/// that stays `0` for as long as no provider has reported one, which is the same
/// reading [`crate::Usage`] gives an unreported figure and so can never trip the
/// bound it would otherwise be checked against.
///
/// A bound that is exactly met has not been exceeded: a remediation sitting on
/// its ceiling still runs, and the stop lands on the figure that went past it.
/// Attempts are asked first, then time, then tokens, so the reason a stopped
/// remediation leaves in the journal is the same one every reader of the same
/// three figures reaches. A ceiling of zero stops at the first refusal, which is
/// how a project that allows no retries at all says so.
#[must_use]
pub fn should_continue(bounds: &Bounds, attempts: u32, elapsed: Duration, tokens: u64) -> Decision {
    if attempts > bounds.max_attempts {
        return Decision::Stop(Bound::Attempts {
            attempts,
            max: bounds.max_attempts,
        });
    }
    if elapsed > bounds.max_elapsed {
        return Decision::Stop(Bound::Elapsed {
            elapsed: elapsed.whole_seconds(),
            max: bounds.max_elapsed.whole_seconds(),
        });
    }
    if let Some(max) = bounds.max_tokens
        && tokens > max
    {
        return Decision::Stop(Bound::Tokens { tokens, max });
    }
    Decision::Continue
}

/// The sentence a refusal quotes when a diff touched a path it was judged by.
const PROTECTED_RULE: &str = "an attempt may not edit the rules it is judged by";

/// The sentence a refusal quotes when a diff path cannot be placed inside the
/// repository, which is the one case this check cannot judge and so is refused
/// rather than passed. See [`check_no_policy_edit`].
const UNLOCATABLE_RULE: &str =
    "a diff path that cannot be located inside the repository is refused rather than trusted";

/// How far one [`Protected`] entry reaches into a diff.
#[derive(Debug, Clone, Copy)]
enum ProtectedKind {
    /// The named directory, the directory itself included, and every path below
    /// it. Guarded at the repository root: `crates/foo/scripts/` is a module,
    /// not this repository's `scripts/`, and a boundary that could not tell the
    /// two apart would be one a task routed around by renaming where it worked.
    Directory,
    /// The named file, wherever it sits. Guarded by name at any depth, because
    /// `clippy` and `rustfmt` read the nearest configuration walking *up* from
    /// the file they are checking: a `clippy.toml` inside one crate is what that
    /// crate's lint gate reads, so a guard fixed to the repository root would
    /// guard half of what it names.
    File,
}

/// One entry of the set no attempt may touch.
#[derive(Debug)]
struct Protected {
    /// The path as the repository writes it: a directory name, or a file name.
    name: &'static str,
    /// How far the entry reaches.
    kind: ProtectedKind,
}

impl Protected {
    /// A directory and everything below it.
    const fn directory(name: &'static str) -> Self {
        Self {
            name,
            kind: ProtectedKind::Directory,
        }
    }

    /// A file, at any depth.
    const fn file(name: &'static str) -> Self {
        Self {
            name,
            kind: ProtectedKind::File,
        }
    }

    /// Whether the lexical components of a diff path fall inside this entry.
    fn covers(&self, parts: &[&OsStr]) -> bool {
        match self.kind {
            ProtectedKind::Directory => parts
                .first()
                .is_some_and(|first| *first == OsStr::new(self.name)),
            ProtectedKind::File => parts
                .last()
                .is_some_and(|last| *last == OsStr::new(self.name)),
        }
    }
}

/// The paths an attempt may not touch, because they are what it is judged by.
///
/// The gate commands themselves live under `scripts/` and the project's own
/// configuration, including the verification profile the runner executes, under
/// `.ktask/` — VISION.md §8 makes both runner-owned rather than agent-owned.
/// The six file names are the configuration the gates in `docs/QUALITY.md` read:
/// lints, format and spelling — each in both spellings its tool accepts — and
/// dependency bans. The set is a table rather than a matched sentence because
/// the rule it encodes has no gradation: an entry is guarded or it is not, and a
/// test walks it entry by entry.
static PROTECTED_PATHS: [Protected; 8] = [
    Protected::directory("scripts"),
    Protected::directory(".ktask"),
    Protected::file("clippy.toml"),
    Protected::file("rustfmt.toml"),
    Protected::file(".rustfmt.toml"),
    Protected::file("deny.toml"),
    Protected::file("_typos.toml"),
    Protected::file("typos.toml"),
];

/// The components a diff path arrives with, and whether it can be placed inside
/// the repository at all.
///
/// `.` components go, and every `..` cancels the component before it, because a
/// path is compared with the protected set by what it points at and not by how
/// it was spelled: `scripts/inner/../quality.sh` is `scripts/quality.sh`, and a
/// check that compared raw strings would let an agent edit the gate script
/// through a spelling of its own choosing.
///
/// The second half is `false` for the three paths this function cannot locate:
/// one that arrives absolute, one that climbs above the root the protected set
/// is rooted at, and one that names no component at all — `""` and `.` are the
/// repository, and the repository contains `scripts/`. Nothing here touches the
/// filesystem, so a path that resolves through a symlink is the caller's to
/// judge, which is why the paths that reach this function come from `git`.
fn lexical(path: &Path) -> (Vec<&OsStr>, bool) {
    let mut parts: Vec<&OsStr> = Vec::new();
    let mut inside = true;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    inside = false;
                }
            }
            Component::RootDir | Component::Prefix(_) => inside = false,
        }
    }
    let placed = inside && !parts.is_empty();
    (parts, placed)
}

/// The protected entry a path's components fall inside, if any.
fn protected_for(parts: &[&OsStr]) -> Option<&'static Protected> {
    PROTECTED_PATHS.iter().find(|entry| entry.covers(parts))
}

/// Refuse an attempt whose diff touched the rules it is judged by.
///
/// `diff_paths` is what the repository says changed — the paths
/// [`crate::git`] reads out of `git diff`, repository-relative and unattributed
/// to anybody's account of the work. VISION.md §3's invariant 5 says self-healing
/// cannot weaken checks or change policy, and §7 repeats it as "never edits gate
/// definitions"; the sentence is only worth what the check behind it is, and an
/// agent that can edit `clippy.toml`, `scripts/quality.sh` or `.ktask/` has
/// rewritten the examination rather than answered it. So does an agent on its
/// first attempt, which is why this is called for every attempt and why it takes
/// no attempt number: the rule is one predicate, and an attempt that had to move
/// a gate to pass it has proven nothing by passing.
///
/// # Paths, not contents
///
/// The check is about which file changed, not what changed in it. A protected
/// path is guarded in full — a one-character edit to `clippy.toml` is as much a
/// rewrite of the lint gate as a deletion is, and so is a deletion, which is why
/// a protected path is refused wherever a diff lists it, added, modified,
/// renamed or deleted alike. `Cargo.toml` is deliberately *not* guarded: it is
/// where `[workspace.lints]` lives (ADR-0067), and it is also the file every
/// dependency change has to edit, which AGENTS.md then requires be committed in
/// the same commit as the lockfile.
///
/// A path this check cannot place inside the repository — absolute, climbing
/// above the root, or naming nothing — is refused too, under its own sentence.
/// The alternative is a boundary that passes whatever it could not read, which
/// is the shape of the hole this whole module exists to close.
///
/// # Errors
///
/// [`Error::Policy`] naming every offending path, in the order the diff listed
/// them, with the rule sentence (or both, when a path breaks two) as the detail.
/// The class is `policy_failure` by the taxonomy of VISION.md §7 — "forbidden
/// file, dirty tree, attempted gate bypass" — and `classify` reaches the same
/// class for the same error, remediation or not.
pub fn check_no_policy_edit(diff_paths: &[PathBuf]) -> Result<()> {
    let mut offenders = Vec::new();
    let mut protected = false;
    let mut unlocatable = false;
    for path in diff_paths {
        let (parts, placed) = lexical(path);
        let touched = protected_for(&parts).is_some();
        if !placed || touched {
            offenders.push(path.clone());
            protected |= touched;
            unlocatable |= !placed;
        }
    }
    if offenders.is_empty() {
        return Ok(());
    }
    let mut rules = Vec::new();
    if protected {
        rules.push(PROTECTED_RULE);
    }
    if unlocatable {
        rules.push(UNLOCATABLE_RULE);
    }
    Err(Error::Policy {
        detail: rules.join("; "),
        paths: offenders,
    })
}

/// The journal record a protected-path refusal leaves behind.
///
/// [`trip_event`] has the same shape and for the same reason: a refusal that
/// lets a task carry on is not a refusal, and `TaskFailed` is the catalog entry
/// that ends it, from `Running` and from `Remediating` alike (ADR-0022). The
/// class is not a parameter, unlike the trip's: VISION.md §7 names
/// [`FailureClass::PolicyFailure`] for a forbidden file or an attempted gate
/// bypass, and the response that class selects has to be the same whether the
/// attempt that touched the path was the first one or the remediation of it.
///
/// `offending_paths` is the list the refusal named — the `paths` of
/// [`check_no_policy_edit`]'s error, not the whole diff — so the record sends a
/// human to the files that broke the rule and to no others.
#[must_use]
pub fn policy_edit_event(offending_paths: &[PathBuf]) -> EventKind {
    let named: Vec<String> = offending_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    EventKind::TaskFailed {
        class: FailureClass::PolicyFailure,
        detail: format!("{PROTECTED_RULE}: {}", named.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::RULES;
    use super::{Bound, Bounds, Decision, should_continue};
    use super::{Breaker, BreakerState, bundle, signature, trip_event};
    use super::{PROTECTED_PATHS, ProtectedKind, check_no_policy_edit, policy_edit_event};
    use crate::{
        AttemptId, AttemptRecord, Error, EventKind, FailureClass, GateKind, GateResult, Journal,
        Phase, Task, TaskId, TaskState, TaskStatus, Usage, UsageSource, apply, journal_path,
        redact::MASK,
    };
    use proptest::prelude::*;
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};
    use time::Duration;
    use time::macros::datetime;

    /// The signature's documented shape: this many lowercase hex characters.
    const HEX_CHARS: usize = 16;

    /// A scratch parent for a journal: `docs/DESIGN.md` Conventions forbids a
    /// test from writing inside the repository.
    fn scratch() -> TempDir {
        tempdir().expect("a scratch directory below the system temp directory")
    }

    /// One gate that refused, with the two numbers a rerun is sure to change
    /// handed in: the runner's own stopwatch and the duration the tool printed.
    fn refused(kind: GateKind, stdout: &str, stderr: &str, duration_ms: u64) -> GateResult {
        GateResult {
            kind,
            passed: false,
            exit_code: Some(101),
            signal: None,
            duration_ms,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            timed_out: false,
        }
    }

    /// A cargo verify run that refused, listing `failures` twice as libtest
    /// does — once as headings full of panic text, once as the summary's own
    /// list of names.
    fn refused_verify(duration_ms: u64, finished_in: &str, failures: &[&str]) -> GateResult {
        use std::fmt::Write as _;

        let mut stdout = format!("running {} tests\n", failures.len() + 1);
        for name in failures {
            writeln!(stdout, "test {name} ... FAILED")
                .expect("a String always has room for what is written into it");
        }
        stdout.push_str("test suite::passes ... ok\n\nfailures:\n");
        for name in failures {
            write!(
                stdout,
                "\n---- {name} stdout ----\nthread 'main' panicked at src/lib.rs:1:1: assertion \
                 failed: left == right\n"
            )
            .expect("a String always has room for what is written into it");
        }
        stdout.push_str("\nfailures:\n");
        for name in failures {
            writeln!(stdout, "    {name}")
                .expect("a String always has room for what is written into it");
        }
        write!(
            stdout,
            "\ntest result: FAILED. 1 passed; {} failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in {finished_in}\n",
            failures.len()
        )
        .expect("a String always has room for what is written into it");
        refused(GateKind::Verify, &stdout, "", duration_ms)
    }

    /// A gate whose command refused without naming a single test: a build or a
    /// lint, whose first line on stderr is the tool's own verdict.
    fn refused_tool(duration_ms: u64, verdict: &str) -> GateResult {
        refused(GateKind::Build, "", &format!("{verdict}\n"), duration_ms)
    }

    /// A gate that ran, was satisfied, and printed a great deal about it.
    fn passed_lint(duration_ms: u64) -> GateResult {
        GateResult {
            kind: GateKind::Lint,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms,
            stdout: String::new(),
            stderr: "Finished in 3.2s across 400 files\n".to_owned(),
            timed_out: false,
        }
    }

    /// Three runs of one failing test, recorded by a breaker of `threshold`.
    fn tripped(threshold: u32) -> (String, BreakerState) {
        let runs = [
            refused_verify(1_204, "0.41s", &["gate::tests::refuses"]),
            refused_verify(88_004, "12.05s", &["gate::tests::refuses"]),
            refused_verify(4_120_004, "1_120.9s", &["gate::tests::refuses"]),
        ];
        let mut breaker = Breaker::new(threshold);
        let mut first = String::new();
        let mut state = BreakerState::Allowed { seen: 0 };
        for (index, run) in runs.iter().enumerate() {
            let sig = signature(FailureClass::VerificationFailure, std::slice::from_ref(run));
            if index == 0 {
                first = sig.clone();
            }
            state = breaker.record(&sig);
        }
        (first, state)
    }

    /// A rule that never compiled is a normalization step that quietly stopped
    /// existing, and the failure it used to describe would then look like a
    /// different failure every time it came back.
    #[test]
    fn every_normalization_rule_compiles() {
        for rule in RULES {
            assert!(
                rule.regex().is_some(),
                "`{}` is not a regular expression",
                rule.source,
            );
        }
    }

    #[test]
    fn one_failing_test_keeps_one_signature_when_only_the_clock_moved() {
        let fast = refused_verify(1_842, "0.31s", &["remediate::tests::records_a_trip"]);
        let slow = refused_verify(
            9_121_004,
            "1_520.19s",
            &["remediate::tests::records_a_trip"],
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[fast]),
            signature(FailureClass::VerificationFailure, &[slow]),
            "one test refusing twice is one failure however long each run took",
        );
    }

    #[test]
    fn two_failing_tests_are_two_failures() {
        let alpha = refused_verify(900, "0.2s", &["gate::tests::alpha_refuses"]);
        let beta = refused_verify(900, "0.2s", &["gate::tests::beta_refuses"]);
        assert_ne!(
            signature(FailureClass::VerificationFailure, &[alpha]),
            signature(FailureClass::VerificationFailure, &[beta]),
            "two different tests must not spend each other's breaker budget",
        );
    }

    #[test]
    fn the_class_is_part_of_the_signature() {
        let gates = [refused_verify(900, "0.2s", &["gate::tests::alpha_refuses"])];
        assert_ne!(
            signature(FailureClass::VerificationFailure, &gates),
            signature(FailureClass::EnvironmentFailure, &gates),
            "the same test refusing for two different reasons earns two responses",
        );
    }

    #[test]
    fn two_instances_of_one_parametrised_test_are_one_failure() {
        let first = refused_verify(700, "0.1s", &["queue::tests::round_trips_case_1"]);
        let later = refused_verify(701, "0.1s", &["queue::tests::round_trips_case_12"]);
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[first]),
            signature(FailureClass::VerificationFailure, &[later]),
            "one body of code with two inputs is one failure",
        );
    }

    #[test]
    fn a_check_out_directory_that_moved_does_not_rename_the_failure() {
        let local = refused_tool(
            1_000,
            "error: /home/alice/wt/crates/ktask-core/src/gate.rs:41:5: unused import `std::fs`",
        );
        let worker = refused_tool(
            60_000,
            "error: /home/ci/agent-7/checkout/crates/ktask-core/src/gate.rs:412:9: unused \
             import `std::fs`",
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[local]),
            signature(FailureClass::VerificationFailure, &[worker]),
            "one file in two checkouts is one failure",
        );
    }

    #[test]
    fn a_refusal_that_only_took_longer_is_the_same_refusal() {
        let quick = refused_tool(1_000, "error: the privacy scan took 3s to decide");
        let slow = refused_tool(4_200_000, "error: the privacy scan took 4200ms to decide");
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[quick]),
            signature(FailureClass::VerificationFailure, &[slow]),
            "a duration is not part of what refused",
        );
    }

    /// A remediation attempt edits the file, so the position a tool reports moves
    /// even when the complaint is word for word the same one. Collapsing what
    /// digit-stripping leaves behind is what keeps the two one failure: `…:41:5:`
    /// and `…:7:` strip to different runs of colons, and only the last rule makes
    /// them the same word.
    #[test]
    fn the_line_a_refusal_was_reported_on_is_not_part_of_the_failure() {
        let before = refused_tool(
            1_000,
            "error: unused import `std::fs`: src/gate.rs:41:5: module never used",
        );
        let after = refused_tool(
            1_900,
            "error: unused import `std::fs`: src/gate.rs:7: module never used",
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[before]),
            signature(FailureClass::VerificationFailure, &[after]),
            "one complaint reported at two positions is one failure",
        );
    }

    /// The verdict is read from stderr because that is where a build or a lint
    /// puts it, and stdout is whatever the command happened to say while it
    /// worked. A run that narrated itself differently is the refusal it was.
    #[test]
    fn the_verdict_of_a_nameless_refusal_is_read_from_stderr() {
        let loud = refused(
            GateKind::Build,
            "warning: 12 files scanned\n",
            "error[E0308]: mismatched types\n",
            800,
        );
        let quiet = refused(
            GateKind::Build,
            "note: the cache was cold\n",
            "error[E0308]: mismatched types\n",
            44_000,
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[loud]),
            signature(FailureClass::VerificationFailure, &[quiet]),
            "two runs of one build failure that chatted differently are one failure",
        );
    }

    #[test]
    fn two_refusals_that_say_different_things_are_two_failures() {
        let types = refused_tool(1_000, "error[E0308]: mismatched types");
        let import = refused_tool(1_000, "error[E0432]: unresolved import `std::fs`");
        assert_ne!(
            signature(FailureClass::VerificationFailure, &[types]),
            signature(FailureClass::VerificationFailure, &[import]),
            "a nameless refusal must not be one signature that trips across unrelated work",
        );
    }

    #[test]
    fn the_order_two_failures_were_reported_in_changes_nothing() {
        let one = refused_verify(500, "0.5s", &["a::tests::alpha", "b::tests::beta"]);
        let other = refused_verify(500, "0.5s", &["b::tests::beta", "a::tests::alpha"]);
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[one]),
            signature(FailureClass::VerificationFailure, &[other]),
            "libtest lists its failures in whatever order they finished",
        );
    }

    #[test]
    fn two_refusing_gates_are_one_failure_whichever_ran_first() {
        let verify = refused_verify(500, "0.5s", &["a::tests::alpha"]);
        let build = refused_tool(60, "error[E0308]: mismatched types");
        assert_eq!(
            signature(
                FailureClass::VerificationFailure,
                &[verify.clone(), build.clone()],
            ),
            signature(FailureClass::VerificationFailure, &[build, verify]),
            "which gate refused first is cargo's scheduling, not the failure",
        );
    }

    #[test]
    fn one_test_that_two_binaries_reported_is_still_one_failure() {
        let single = refused_verify(500, "0.5s", &["gate::tests::refuses"]);
        let doubled = refused(
            GateKind::Verify,
            &format!("{}{}", single.stdout, single.stdout),
            "",
            4_000_000,
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[single]),
            signature(FailureClass::VerificationFailure, &[doubled]),
            "one test reported by two binaries has not become two failures",
        );
    }

    #[test]
    fn a_gate_that_passed_contributes_nothing() {
        let verify = refused_verify(500, "0.5s", &["gate::tests::refuses"]);
        assert_eq!(
            signature(
                FailureClass::VerificationFailure,
                std::slice::from_ref(&verify)
            ),
            signature(
                FailureClass::VerificationFailure,
                &[verify, passed_lint(72_000_000)],
            ),
            "a green gate says nothing about what refused",
        );
    }

    #[test]
    fn a_silent_refusal_is_named_by_the_gate_that_refused() {
        assert_eq!(
            signature(
                FailureClass::EnvironmentFailure,
                &[refused(GateKind::Verify, "", "", 10)],
            ),
            signature(
                FailureClass::EnvironmentFailure,
                &[refused(GateKind::Verify, "", "", 9_999)],
            ),
            "a gate that refused without a word still repeats itself",
        );
        assert_ne!(
            signature(
                FailureClass::EnvironmentFailure,
                &[refused(GateKind::Verify, "", "", 10)],
            ),
            signature(
                FailureClass::EnvironmentFailure,
                &[refused(GateKind::Lint, "", "", 10)],
            ),
            "two gates that both refused in silence are not one failure",
        );
    }

    #[test]
    fn a_signature_is_hex_of_the_documented_length() {
        let sig = signature(FailureClass::AgentFailure, &[]);
        assert_eq!(sig.chars().count(), HEX_CHARS, "{sig} is the wrong length");
        assert!(
            sig.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{sig} is not lowercase hexadecimal",
        );
    }

    #[test]
    fn a_failure_that_named_nothing_is_told_apart_by_its_class() {
        assert_ne!(
            signature(FailureClass::AgentFailure, &[]),
            signature(FailureClass::PolicyFailure, &[]),
            "a class is part of the answer even when no gate said a word",
        );
    }

    proptest! {
        /// The stopwatch is not part of the failure, at any two readings.
        #[test]
        fn any_two_durations_of_one_refusal_are_one_failure(
            first in any::<u64>(),
            second in any::<u64>(),
        ) {
            let quick = refused_verify(first, "0.11s", &["gate::tests::refuses"]);
            let slow = refused_verify(second, "9.99s", &["gate::tests::refuses"]);
            prop_assert_eq!(
                signature(FailureClass::VerificationFailure, &[quick]),
                signature(FailureClass::VerificationFailure, &[slow]),
            );
        }

        /// No transcript, however strange, yields a signature of another shape.
        #[test]
        fn a_signature_is_always_hex(text in "[\\x20-\\x7e]{0,200}", ms in any::<u64>()) {
            let gate = refused(GateKind::Privacy, &text, &text, ms);
            let sig = signature(FailureClass::PolicyFailure, &[gate]);
            prop_assert_eq!(sig.chars().count(), HEX_CHARS, "{} is the wrong length", sig);
            prop_assert!(
                sig.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{} is not lowercase hexadecimal", sig,
            );
        }
    }

    #[test]
    fn the_breaker_trips_when_one_signature_reaches_the_threshold() {
        let mut breaker = Breaker::new(3);
        assert_eq!(
            breaker.record("aaaa111122223333"),
            BreakerState::Allowed { seen: 1 }
        );
        assert_eq!(
            breaker.record("aaaa111122223333"),
            BreakerState::Allowed { seen: 2 }
        );
        assert_eq!(
            breaker.record("aaaa111122223333"),
            BreakerState::Tripped {
                signature: "aaaa111122223333".to_owned(),
                seen: 3,
            },
        );
    }

    #[test]
    fn another_signature_starts_a_count_of_its_own() {
        let mut breaker = Breaker::new(3);
        assert_eq!(breaker.record("one"), BreakerState::Allowed { seen: 1 });
        assert_eq!(breaker.record("two"), BreakerState::Allowed { seen: 1 });
        assert_eq!(breaker.record("one"), BreakerState::Allowed { seen: 2 });
        assert_eq!(breaker.record("two"), BreakerState::Allowed { seen: 2 });
        assert!(
            matches!(breaker.record("one"), BreakerState::Tripped { seen: 3, .. }),
            "a failure that alternates with a different one is still the same \
             failure returning",
        );
    }

    #[test]
    fn a_tripped_breaker_does_not_close_again() {
        let mut breaker = Breaker::new(1);
        assert_eq!(
            breaker.record("aaaa111122223333"),
            BreakerState::Tripped {
                signature: "aaaa111122223333".to_owned(),
                seen: 1,
            },
        );
        assert!(
            matches!(
                breaker.record("bbbb111122223333"),
                BreakerState::Tripped { .. },
            ),
            "a breaker that re-closes on a differently worded failure is a counter",
        );
    }

    #[test]
    fn three_runs_of_one_failing_test_trip_the_breaker() {
        let (first, last) = tripped(3);
        assert_eq!(
            last,
            BreakerState::Tripped {
                signature: first,
                seen: 3,
            },
            "three runs of one test that only differed by their durations are \
             three records of one signature",
        );
    }

    #[test]
    fn two_runs_of_one_failing_test_do_not_trip_a_threshold_of_three() {
        let mut breaker = Breaker::new(3);
        for run in [
            refused_verify(1_204, "0.41s", &["gate::tests::refuses"]),
            refused_verify(88_004, "12.05s", &["gate::tests::refuses"]),
        ] {
            let sig = signature(FailureClass::VerificationFailure, &[run]);
            let state = breaker.record(&sig);
            assert!(
                matches!(state, BreakerState::Allowed { seen: 1 | 2 }),
                "one failure short of the threshold is not a trip: {state:?}",
            );
        }
    }

    #[test]
    fn the_trip_is_journaled_as_the_tasks_failure() {
        let (sig, state) = tripped(3);
        let BreakerState::Tripped { signature, seen } = state else {
            panic!("three identical records must trip: {state:?}");
        };
        let dir = scratch();
        let state_dir = dir.path().join("state-7");
        std::fs::create_dir(&state_dir).expect("a state directory the journal may live in");
        let task = TaskId::new(7);
        let mut journal = Journal::open(&journal_path(&state_dir)).expect("a journal opens in it");
        journal
            .append(
                Some(task),
                &trip_event(FailureClass::VerificationFailure, &signature, seen),
            )
            .expect("a trip is written before anything else happens");

        let rows = journal.events_for(task).expect("the journal reads back");
        assert_eq!(rows.len(), 1, "one trip is one record");
        assert_eq!(rows[0].kind.discriminant(), "TaskFailed");
        let EventKind::TaskFailed { class, detail } = &rows[0].kind else {
            panic!(
                "a trip is journalled as the task's failure: {:?}",
                rows[0].kind
            );
        };
        assert_eq!(*class, FailureClass::VerificationFailure);
        assert!(
            detail.contains(&signature) && detail.contains(&seen.to_string()),
            "the record has to say which signature tripped and how often: {detail}",
        );
        assert_eq!(
            signature, sig,
            "the journal holds the signature the runs gave"
        );

        let projected = apply(
            &TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            },
            &rows[0].kind,
        )
        .expect("a task under remediation can be failed by a trip");
        assert_eq!(
            projected,
            TaskState::Failed {
                class: FailureClass::VerificationFailure,
                detail: detail.clone(),
            },
        );
    }

    /// The task every bundle here is assembled for: task 7 of this queue.
    fn task_7() -> Task {
        Task {
            id: TaskId::new(7),
            status: TaskStatus::Pending,
            body: "## T069 Failure bundle assembly\n\n**Outcome:** a compact bundle.\n".to_owned(),
            outcome: "remediation starts from a compact, deterministic bundle".to_owned(),
            done_when: "the same failure produces a byte-identical bundle".to_owned(),
            verify: "cargo nextest run -p ktask-core -E 'test(/remediate::/)'".to_owned(),
            refs: "VISION.md section 7".to_owned(),
            gate: None,
            protocol: None,
        }
    }

    /// One prior attempt of task 7: the `number`th run, which refused `refused`,
    /// stopped with `reason`, and produced `candidate` if it produced a commit.
    fn prior_attempt(
        number: u32,
        kinds: &[GateKind],
        candidate: Option<&str>,
        reason: &str,
        usage: Option<Usage>,
    ) -> AttemptRecord {
        AttemptRecord {
            id: AttemptId::new(number),
            task: TaskId::new(7),
            started: datetime!(2026-09-20 09:14:03 UTC),
            ended: Some(datetime!(2026-09-20 09:41:47 UTC)),
            model_configured: Some("gpt-5.6-sol".to_owned()),
            model_reported: Some("gpt-5.6-sol-2026-09-01".to_owned()),
            session_id: Some("sess_01HQZK".to_owned()),
            exit_reason: reason.to_owned(),
            gates: kinds
                .iter()
                .map(|kind| refused(*kind, "", "the gate refused\n", 40))
                .collect(),
            usage,
            base_sha: "0b78d3f1c2a4".to_owned(),
            candidate_sha: candidate.map(str::to_owned),
        }
    }

    /// The failure every budget is priced against: two gates that refused (build
    /// first, then verify), a diff summary, and two prior attempts. Every line of
    /// it carries a marker of its own, so where a budget cut a bundle is read
    /// straight off the bundle.
    fn failure() -> (Vec<GateResult>, String, Vec<AttemptRecord>) {
        let gates = vec![
            refused(
                GateKind::Build,
                "",
                "GATE-ONE-OLDEST\nGATE-ONE-NEWEST\n",
                900,
            ),
            refused(
                GateKind::Verify,
                "GATE-TWO-OLDEST\nGATE-TWO-NEWEST\n",
                "",
                1_400,
            ),
        ];
        let diff = "DIFF-OLDEST\nDIFF-NEWEST\n".to_owned();
        let attempts = vec![
            prior_attempt(
                1,
                &[GateKind::Verify],
                Some("aaaaaaaaaaaa"),
                "PRIOR-ONE",
                None,
            ),
            prior_attempt(
                2,
                &[GateKind::Verify, GateKind::Build],
                None,
                "PRIOR-TWO",
                Some(Usage::unavailable()),
            ),
        ];
        (gates, diff, attempts)
    }

    /// [`failure`] trimmed to `budget` bytes.
    fn priced_at(budget: usize) -> String {
        let (gates, diff, attempts) = failure();
        bundle(
            &task_7(),
            FailureClass::VerificationFailure,
            &gates,
            &diff,
            &attempts,
            budget,
        )
    }

    /// [`failure`] with a budget nothing could exceed.
    fn complete() -> String {
        priced_at(usize::MAX)
    }

    /// The smallest budget whose bundle still holds `marker`.
    ///
    /// A budget is the only way to ask which evidence a bundle loses first: a
    /// marker that appears at a smaller budget is the evidence a tight
    /// remediation session is left with.
    fn budget_holding(marker: &str) -> usize {
        let ceiling = complete().len();
        (0..=ceiling)
            .find(|budget| priced_at(*budget).contains(marker))
            .unwrap_or_else(|| panic!("`{marker}` is nowhere in the {ceiling}-byte whole bundle"))
    }

    /// The three shapes a secret arrives in, one planted in each kind of evidence
    /// a bundle is assembled from.
    fn secret_failure() -> (Vec<GateResult>, String, Vec<AttemptRecord>) {
        let gates = vec![refused(
            GateKind::Verify,
            "BEFORE-THE-LEAK\nOPENAI_KEY=sk-proj-AAAAAAAAAAAAAAAAAAAAAAAAAAAA\nAFTER-THE-LEAK\n",
            "Authorization: Bearer supersecretvalue123\n",
            900,
        )];
        let diff =
            "added a line: aws_secret_access=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n".to_owned();
        let attempts = vec![prior_attempt(
            1,
            &[GateKind::Verify],
            None,
            "pull https://gitlab-ci-token:glpat-aaaaaaaaaaaaaaaaaaaa@corp.example/repo refused",
            None,
        )];
        (gates, diff, attempts)
    }

    /// The values [`secret_failure`] plants, each of which must be gone from every
    /// bundle of it at every budget.
    const SECRETS: [&str; 4] = [
        "sk-proj-AAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "supersecretvalue123",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "glpat-aaaaaaaaaaaaaaaaaaaa",
    ];

    /// [`secret_failure`] trimmed to `budget` bytes.
    fn secret_priced_at(budget: usize) -> String {
        let (gates, diff, attempts) = secret_failure();
        bundle(
            &task_7(),
            FailureClass::PolicyFailure,
            &gates,
            &diff,
            &attempts,
            budget,
        )
    }

    /// The bundle every gate-only test assembles: [`task_7`], one class, and
    /// `gates` as the whole of the run's evidence.
    fn one_run(gates: &[GateResult]) -> String {
        bundle(
            &task_7(),
            FailureClass::VerificationFailure,
            gates,
            "",
            &[],
            usize::MAX,
        )
    }

    /// The bundle `attempts` as the only prior evidence.
    fn one_history(attempts: &[AttemptRecord]) -> String {
        bundle(
            &task_7(),
            FailureClass::AgentFailure,
            &[],
            "",
            attempts,
            usize::MAX,
        )
    }

    #[test]
    fn a_bundle_holds_the_classification_the_gate_output_the_diff_and_the_history() {
        let text = complete();
        assert!(
            text.contains("class: VerificationFailure"),
            "the class is what decides the response, so it has to be in the bundle: {text}"
        );
        assert!(
            text.contains("task 7: ## T069 Failure bundle assembly"),
            "a bundle has to name the task it is a failure of: {text}"
        );
        assert!(
            text.contains("GATE-ONE-OLDEST") && text.contains("GATE-TWO-NEWEST"),
            "every refusing gate is evidence a fresh session cannot recompute: {text}"
        );
        assert!(
            text.contains("DIFF-OLDEST") && text.contains("DIFF-NEWEST"),
            "the diff summary is what the session was last told about the tree: {text}"
        );
        assert!(
            text.contains("PRIOR-ONE") && text.contains("PRIOR-TWO"),
            "prior attempt outcomes are what stops a remediation retrying a fix that \
             already failed: {text}"
        );
        assert!(
            text.contains("usage unasked") && text.contains("usage unreported"),
            "an attempt's cost is part of its outcome: {text}"
        );
    }

    #[test]
    fn one_failure_renders_one_bundle_whatever_order_the_attempts_arrived_in() {
        let (gates, diff, mut reversed) = failure();
        reversed.reverse();
        let (_gates, _diff, ordered) = failure();
        assert_eq!(
            bundle(
                &task_7(),
                FailureClass::VerificationFailure,
                &gates,
                &diff,
                &reversed,
                usize::MAX,
            ),
            bundle(
                &task_7(),
                FailureClass::VerificationFailure,
                &gates,
                &diff,
                &ordered,
                usize::MAX,
            ),
            "evidence is filed when an attempt's recorder reaches it, so the same failure \
             handed over in another order is one bundle, not two",
        );
    }

    #[test]
    fn one_failure_renders_one_bundle_when_only_the_clock_and_the_session_moved() {
        let (gates, diff, attempts) = failure();
        let mut rerun_gates = gates.clone();
        for gate in &mut rerun_gates {
            gate.duration_ms = gate.duration_ms.saturating_add(9_120_004);
        }
        let mut rerun_attempts = attempts.clone();
        for attempt in &mut rerun_attempts {
            attempt.started = datetime!(2026-09-21 22:47:19 UTC);
            attempt.ended = Some(datetime!(2026-09-22 03:12:55 UTC));
            attempt.session_id = Some("sess_01ZZZZ".to_owned());
            attempt.model_reported = Some("gpt-5.6-sol-2026-10-01".to_owned());
        }
        assert_eq!(
            bundle(
                &task_7(),
                FailureClass::VerificationFailure,
                &gates,
                &diff,
                &attempts,
                usize::MAX,
            ),
            bundle(
                &task_7(),
                FailureClass::VerificationFailure,
                &rerun_gates,
                &diff,
                &rerun_attempts,
                usize::MAX,
            ),
            "a stopwatch reading and a session id are the two things a rerun of one failure \
             always changes, and a bundle that carried them would never match itself",
        );
    }

    #[test]
    fn a_gate_that_was_satisfied_contributes_no_output() {
        let text = one_run(&[
            refused(GateKind::Build, "", "BUILD-REFUSED\n", 900),
            passed_lint(72_000),
        ]);
        assert!(
            text.contains("BUILD-REFUSED"),
            "the gate that refused has to be in it: {text}"
        );
        assert!(
            !text.contains("Finished in 3.2s"),
            "a green gate's chatter is not failure evidence and spends the budget it would \
             have paid for a refusal: {text}"
        );
        assert!(
            text.contains("gates refused: build"),
            "the frame lists which gates refused: {text}"
        );
        assert!(
            !text.contains("lint"),
            "a satisfied gate is not named as a refusal: {text}"
        );
    }

    #[test]
    fn a_refusal_that_wrote_nothing_says_so() {
        let text = one_run(&[refused(GateKind::Privacy, "", "", 30)]);
        assert!(
            text.contains("failing gate privacy (exit 101)"),
            "a refusal with no output still names its gate: {text}"
        );
        assert!(
            text.contains("without writing"),
            "an empty transcript is a fact about the refusal, not an empty block: {text}"
        );
    }

    #[test]
    fn a_timed_out_gate_is_named_by_its_timeout_not_the_signal_that_stopped_it() {
        let mut timed = refused(GateKind::Verify, "still running\n", "", 1_800_000);
        timed.timed_out = true;
        timed.exit_code = None;
        timed.signal = Some(9);
        let text = one_run(&[timed]);
        assert!(
            text.contains("failing gate verify (timed out)"),
            "a timeout is its own fact and the kill that enforced it is not the failure: {text}"
        );

        let mut killed = refused(GateKind::Lint, "", "killed mid-run\n", 4_000);
        killed.exit_code = None;
        killed.signal = Some(11);
        let text = one_run(&[killed]);
        assert!(
            text.contains("failing gate lint (signal 11)"),
            "a gate stopped by a signal has no exit code to name: {text}"
        );
    }

    #[test]
    fn a_gate_stopped_with_neither_a_code_nor_a_signal_says_only_that() {
        let mut stalled = refused(GateKind::Format, "", "checked 400 files\n", 4_000);
        stalled.exit_code = None;
        stalled.signal = None;
        let text = one_run(&[stalled]);
        assert!(
            text.contains("failing gate format (stopped without a verdict)"),
            "a gate that reported no code and no signal is a third kind of stop, and the \
             bundle may not guess which of the other two it was: {text}"
        );
        assert!(
            text.contains("checked 400 files"),
            "what it did write is still the evidence: {text}"
        );
    }

    #[test]
    fn a_figure_that_was_not_reported_is_not_written_as_a_zero() {
        let text = one_history(&[
            prior_attempt(1, &[GateKind::Verify], None, "PRIOR-ONE", None),
            prior_attempt(
                2,
                &[GateKind::Verify],
                None,
                "PRIOR-TWO",
                Some(Usage::unavailable()),
            ),
        ]);
        assert!(
            text.contains("usage unasked"),
            "an attempt with no session to ask is told apart from one that asked and was \
             told nothing: {text}"
        );
        assert!(
            text.contains("usage unreported"),
            "a session that reported nothing is not a session that spent nothing: {text}"
        );
        for substituted in ["in=0", "out=0", "cached=0", "cost=$0"] {
            assert!(
                !text.contains(substituted),
                "`{substituted}` is a zero standing in for a figure nobody reported: {text}"
            );
        }
    }

    #[test]
    fn a_prior_attempt_reports_the_commit_it_produced_and_what_it_spent() {
        let spent = Usage {
            input_tokens: Some(12_000),
            output_tokens: Some(3_400),
            cached_tokens: None,
            cost_usd: Some(0.42),
            source: UsageSource::Provider,
        };
        let text = one_history(&[
            prior_attempt(
                1,
                &[GateKind::Verify],
                Some("b7d1f3a9e5c2"),
                "PRIOR-ONE",
                Some(spent),
            ),
            prior_attempt(2, &[], None, "PRIOR-TWO", Some(Usage::unavailable())),
        ]);
        assert!(
            text.contains("produced b7d1f3a9e5c2"),
            "what an attempt committed is where a remediation starts reading: {text}"
        );
        assert!(
            text.contains("in=12000") && text.contains("out=3400") && text.contains("cost=$0.42"),
            "the figures a session did report are its outcome: {text}"
        );
        assert!(
            !text.contains("cached="),
            "a figure that was never reported is left out rather than guessed: {text}"
        );
        assert!(
            text.contains("produced no commit"),
            "an attempt that committed nothing says so: {text}"
        );
        assert!(
            text.contains("refused nothing"),
            "an attempt whose gates were all satisfied still failed for another reason, and \
             the bundle says which half of the run was green: {text}"
        );
    }

    #[test]
    fn a_tight_bundle_keeps_the_newest_evidence_and_loses_the_oldest() {
        assert!(
            budget_holding("GATE-TWO-NEWEST") < budget_holding("GATE-TWO-OLDEST"),
            "a gate's tail is the part a remediation reads, so its head is what goes first",
        );
        assert!(
            budget_holding("GATE-TWO-OLDEST") < budget_holding("DIFF-NEWEST"),
            "the gate that refused is the newest evidence there is: it survives a diff",
        );
        assert!(
            budget_holding("DIFF-NEWEST") < budget_holding("PRIOR-TWO"),
            "this attempt's own diff is newer than the attempt before it",
        );
        assert!(
            budget_holding("PRIOR-TWO") < budget_holding("PRIOR-ONE"),
            "of two prior attempts the oldest is the one a bounded bundle loses",
        );
        assert!(
            budget_holding("GATE-TWO-NEWEST") < budget_holding("GATE-ONE-NEWEST"),
            "the gate that ran last is the newest refusal, so the refusal before it is what a \
             tight budget spends first",
        );
    }

    #[test]
    fn a_gate_block_loses_its_own_label_before_the_output_it_names() {
        let budget = budget_holding("GATE-TWO-NEWEST");
        let text = priced_at(budget);
        assert!(
            text.contains("GATE-TWO-NEWEST"),
            "the budget was found by looking for this line: {text}"
        );
        assert!(
            !text.contains("failing gate verify"),
            "a block's label is the oldest line of its block, so a trimmed transcript arrives \
             unnamed: {text}"
        );
        assert!(
            text.contains("gates refused: build verify"),
            "which is why the frame names the refusing gates whatever survived of them: {text}"
        );
    }

    #[test]
    fn the_class_is_the_last_thing_a_tight_budget_loses() {
        let class_line = complete()
            .lines()
            .find(|line| line.starts_with("class: "))
            .expect("the frame names the class")
            .to_owned();
        assert_eq!(
            budget_holding(&class_line),
            class_line.len(),
            "the class line is the last thing in the bundle and the first thing a budget \
             that cannot pay for it cuts into",
        );
        let starved = priced_at(class_line.len() - 1);
        assert!(
            !starved.contains("task 7"),
            "a budget too small for the class has already lost the task it names: {starved}"
        );
        assert!(
            class_line.ends_with(&starved),
            "what is left of a starved bundle is the tail of the class line: {starved}"
        );
    }

    #[test]
    fn the_frame_still_counts_the_evidence_a_tight_budget_dropped() {
        let budget = budget_holding("DIFF-OLDEST") - 1;
        let text = priced_at(budget);
        assert!(
            !text.contains("DIFF-OLDEST"),
            "the budget was chosen for having lost this line: {text}"
        );
        assert!(
            !text.contains("PRIOR-ONE") && !text.contains("PRIOR-TWO"),
            "the prior attempts are older than the diff, so they went first: {text}"
        );
        assert!(
            text.contains("prior attempts: 2"),
            "a session handed two attempts of evidence and told there were two cannot tell \
             itself that there were only two: {text}"
        );
        assert!(
            text.contains("gates refused: build verify"),
            "and the same about the gates that refused: {text}"
        );
    }

    #[test]
    fn a_bundle_never_costs_more_than_its_budget() {
        let ceiling = complete().len();
        for budget in 0..=ceiling + 8 {
            let text = priced_at(budget);
            assert!(
                text.len() <= budget,
                "a budget of {budget} bytes cost {}: {text}",
                text.len(),
            );
        }
        assert_eq!(
            priced_at(ceiling),
            complete(),
            "the whole bundle costs exactly what it costs, so a budget that can pay for it \
             loses nothing",
        );
    }

    #[test]
    fn an_empty_budget_answers_an_empty_bundle() {
        assert_eq!(priced_at(0), "", "no budget is no bundle, not a whole one");
    }

    #[test]
    fn a_secret_in_the_evidence_never_reaches_the_bundle() {
        let text = secret_priced_at(usize::MAX);
        for secret in SECRETS {
            assert!(
                !text.contains(secret),
                "`{secret}` survived the redaction a bundle is required to run: {text}"
            );
        }
        assert!(
            text.contains(MASK),
            "a bundle that lost the secret without a mask lost the line instead, which is not \
             the same guarantee: {text}"
        );
        assert!(
            text.contains("BEFORE-THE-LEAK") && text.contains("AFTER-THE-LEAK"),
            "the prose beside a secret is kept: only the value goes: {text}"
        );
    }

    #[test]
    fn no_budget_leaks_a_secret() {
        let ceiling = secret_priced_at(usize::MAX).len();
        for budget in 0..=ceiling + 8 {
            let text = secret_priced_at(budget);
            assert!(
                text.len() <= budget,
                "a budget of {budget} bytes cost {}",
                text.len(),
            );
            for secret in SECRETS {
                assert!(
                    !text.contains(secret),
                    "a cut at {budget} bytes left `{secret}` behind, which is what redacting \
                     before truncating rather than after is there to prevent: {text}"
                );
            }
        }
    }

    proptest! {
        /// No budget is ever exceeded, whatever the evidence holds — including
        /// text whose characters are wider than one byte, where the cut has to
        /// land on a character rather than a byte.
        #[test]
        fn a_bundle_never_costs_more_than_its_budget_prop(
            transcript in any::<String>(),
            reason in any::<String>(),
            budget in 0usize..600,
        ) {
            let gates = vec![refused(GateKind::Verify, &transcript, "", 700)];
            let attempts = vec![prior_attempt(1, &[GateKind::Verify], Some("abc"), &reason, None)];
            let text = bundle(
                &task_7(),
                FailureClass::AgentFailure,
                &gates,
                "DIFF\n",
                &attempts,
                budget,
            );
            prop_assert!(
                text.len() <= budget,
                "a budget of {budget} bytes cost {} bytes",
                text.len(),
            );
        }

        /// The bundle is a projection of the evidence handed to it, so two calls
        /// over equal evidence are the same bytes.
        #[test]
        fn the_same_evidence_renders_the_same_bytes(
            transcript in "[\\x20-\\x7e]{0,300}",
            diff in "[\\x20-\\x7e]{0,120}",
            budget in 0usize..400,
        ) {
            let gates = vec![refused(GateKind::Verify, &transcript, "", 700)];
            let attempts = vec![prior_attempt(1, &[GateKind::Verify], None, "PRIOR", None)];
            let once = bundle(
                &task_7(),
                FailureClass::VerificationFailure,
                &gates,
                &diff,
                &attempts,
                budget,
            );
            let twice = bundle(
                &task_7(),
                FailureClass::VerificationFailure,
                &gates,
                &diff,
                &attempts,
                budget,
            );
            prop_assert_eq!(once, twice);
        }
    }

    /// The bounds every test below varies one figure of: three attempts, a
    /// minute of wall clock, and a token ceiling.
    fn bounds() -> Bounds {
        Bounds {
            max_attempts: 3,
            max_elapsed: Duration::seconds(60),
            max_tokens: Some(1_000),
        }
    }

    /// Nothing is spent yet, so nothing can stop the remediation.
    #[test]
    fn an_unspent_budget_continues() {
        assert_eq!(
            should_continue(&bounds(), 0, Duration::ZERO, 0),
            Decision::Continue
        );
    }

    /// An attempt ceiling of three means three failures were paid for and a
    /// fourth is not. The reason says attempts, and carries both the count it
    /// stopped at and the ceiling.
    #[test]
    fn the_attempt_bound_stops_and_says_so() {
        let stop = should_continue(&bounds(), 4, Duration::seconds(10), 500);
        assert_eq!(
            stop,
            Decision::Stop(Bound::Attempts {
                attempts: 4,
                max: 3
            })
        );
        assert_eq!(stop.to_string(), "attempts 4 past the 3 bound");
    }

    /// Time alone stops the remediation while attempts and tokens sit inside
    /// their ceilings: one session that hung for two minutes costs more wall
    /// clock than three short ones, and the attempt count would never notice.
    ///
    /// The reason says elapsed — not attempts, which a reader would otherwise
    /// go on to raise.
    #[test]
    fn the_elapsed_bound_stops_and_says_so() {
        let stop = should_continue(&bounds(), 1, Duration::seconds(90), 500);
        assert_eq!(
            stop,
            Decision::Stop(Bound::Elapsed {
                elapsed: 90,
                max: 60
            })
        );
        assert_eq!(stop.to_string(), "elapsed 90s past the 60s bound");
    }

    /// A token ceiling spent while the other two bounds sit untouched stops the
    /// remediation on its own, and says tokens.
    #[test]
    fn the_token_bound_stops_and_says_so() {
        let stop = should_continue(&bounds(), 1, Duration::seconds(10), 1_001);
        assert_eq!(
            stop,
            Decision::Stop(Bound::Tokens {
                tokens: 1_001,
                max: 1_000
            })
        );
        assert_eq!(stop.to_string(), "tokens 1001 past the 1000 bound");
    }

    /// Each bound stops the work while the other two are still inside their
    /// ceilings, so a stop is never reported against the wrong figure.
    #[test]
    fn one_bound_stops_without_the_others() {
        let spare = bounds();
        assert!(matches!(
            should_continue(&spare, 4, Duration::ZERO, 0),
            Decision::Stop(Bound::Attempts { .. })
        ));
        assert!(matches!(
            should_continue(&spare, 0, Duration::seconds(61), 0),
            Decision::Stop(Bound::Elapsed { .. })
        ));
        assert!(matches!(
            should_continue(&spare, 0, Duration::ZERO, 1_001),
            Decision::Stop(Bound::Tokens { .. })
        ));
    }

    /// A bound at its exact ceiling has not been exceeded, and all three at
    /// their ceilings still continue. This is the difference between a bound
    /// and an off-by-one, and it is what makes a stop about the figure that
    /// actually went past its ceiling rather than one that merely reached it.
    #[test]
    fn a_bound_exactly_met_has_not_been_spent() {
        assert_eq!(
            should_continue(&bounds(), 3, Duration::seconds(60), 1_000),
            Decision::Continue
        );
    }

    /// An unmeasurable bound stops nothing. A provider that reports no token
    /// figure leaves the count at zero forever, so a loop bounded only by
    /// tokens would run unbounded; `None` is how a caller says no ceiling on
    /// tokens exists at all, and it holds at any spend.
    #[test]
    fn a_bound_that_is_none_stops_nothing() {
        let unbounded = Bounds {
            max_tokens: None,
            ..bounds()
        };
        assert_eq!(
            should_continue(&unbounded, 0, Duration::ZERO, u64::MAX),
            Decision::Continue
        );
    }

    /// An attempt ceiling of zero bounds the loop to nothing: the first
    /// failure already spent it, so a remediation that was allowed no retries
    /// stops before it launches a second session.
    #[test]
    fn an_attempt_ceiling_of_zero_stops_at_the_first_failure() {
        let none_left = Bounds {
            max_attempts: 0,
            ..bounds()
        };
        assert_eq!(
            should_continue(&none_left, 1, Duration::ZERO, 0),
            Decision::Stop(Bound::Attempts {
                attempts: 1,
                max: 0
            })
        );
    }

    /// The first bound past its ceiling is the one reported, in the order
    /// attempts, then time, then tokens — so the reason a stopped remediation
    /// leaves in the journal is the same one every reader of the same three
    /// figures reaches.
    #[test]
    fn the_first_spent_bound_is_the_one_reported() {
        let spent = should_continue(&bounds(), 4, Duration::seconds(90), 2_000);
        assert_eq!(
            spent,
            Decision::Stop(Bound::Attempts {
                attempts: 4,
                max: 3
            })
        );

        let timed_and_billed = should_continue(&bounds(), 1, Duration::seconds(90), 2_000);
        assert_eq!(
            timed_and_billed,
            Decision::Stop(Bound::Elapsed {
                elapsed: 90,
                max: 60
            })
        );
    }

    /// A diff path list spelled the way `git diff --name-only` spells it:
    /// repository-relative, one path per changed file.
    fn diff(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().copied().map(PathBuf::from).collect()
    }

    /// The refusal `check_no_policy_edit` handed back for `paths`, as the rule
    /// sentence it carries and the offending paths it named.
    fn refusal(paths: &[&str]) -> (String, Vec<PathBuf>) {
        let error = check_no_policy_edit(&diff(paths))
            .expect_err("a change to the rules the attempt is judged by is refused");
        match error {
            Error::Policy { detail, paths } => (detail, paths),
            other => panic!("the refusal is a policy violation, not `{other}`"),
        }
    }

    /// The journal record a refusal for `paths` leaves behind, read back out of a
    /// real journal: a refusal that exists only as a return value has stopped
    /// nothing a later reader can see.
    fn journalled(paths: &[&str]) -> (FailureClass, String) {
        let dir = scratch();
        let state_dir = dir.path().join("state-72");
        std::fs::create_dir(&state_dir).expect("a state directory the journal may live in");
        let task = TaskId::new(72);
        let mut journal = Journal::open(&journal_path(&state_dir)).expect("a journal opens in it");
        let (_, offenders) = refusal(paths);
        journal
            .append(Some(task), &policy_edit_event(&offenders))
            .expect("a refusal is journalled before anything else happens");

        let rows = journal.events_for(task).expect("the journal reads back");
        assert_eq!(rows.len(), 1, "one refusal is one record");
        assert_eq!(rows[0].kind.discriminant(), "TaskFailed");
        let EventKind::TaskFailed { class, detail } = &rows[0].kind else {
            panic!(
                "a refusal is journalled as the task's failure: {:?}",
                rows[0].kind
            );
        };
        (*class, detail.clone())
    }

    /// The boundary is about the rules, not about the work: an ordinary change
    /// to source, to a test, to a documentation record and to the manifest all
    /// pass, because a task that could not do those could not do anything.
    #[test]
    fn an_ordinary_change_to_the_project_passes_the_boundary() {
        check_no_policy_edit(&diff(&[
            "crates/ktask-core/src/remediate.rs",
            "crates/ktask-core/tests/durability.rs",
            "docs/adr/0067-a-self-healing-boundary-is-a-path-set.md",
            "README.md",
        ]))
        .expect("ordinary work is what every attempt is for");
    }

    /// An empty diff touches nothing, so it cannot touch a protected path. This
    /// is the state of an attempt that changed no file at all, which the gates
    /// refuse as an empty commit (ADR-0045) but which is no policy violation.
    #[test]
    fn an_empty_diff_touches_nothing_protected() {
        check_no_policy_edit(&[]).expect("no changed path is no protected path");
    }

    /// `Cargo.toml` holds this workspace's `[workspace.lints]`, and is still not
    /// a protected path: AGENTS.md requires a task that adds a dependency to
    /// commit the manifest and `Cargo.lock` with it, so guarding the manifest
    /// would refuse ordinary work. The half of the lint set that lives there is
    /// a named gap in the boundary, reported rather than quietly closed.
    #[test]
    fn the_manifest_that_carries_the_lint_set_is_not_a_protected_path() {
        check_no_policy_edit(&diff(&["Cargo.toml", "Cargo.lock"]))
            .expect("a dependency change is ordinary work, not a policy violation");
    }

    /// A guard is a path, not a substring. `scripts-extra/` is not `scripts/`,
    /// `clippy.toml.bak` is not `clippy.toml`, and a directory named `scripts`
    /// below `crates/` is not the repository's `scripts/` — a boundary that
    /// refused these would be a boundary a task learned to route around by
    /// renaming the directory it worked in.
    #[test]
    fn a_name_that_only_looks_like_a_protected_path_passes() {
        for near in [
            "scripts.rs",
            "scripts-extra/gate.sh",
            "crates/ktask-core/src/scripts/paths.rs",
            "clippy.toml.bak",
            "deny.toml.orig",
            ".ktask-worktrees/main/task-72/report.md",
        ] {
            check_no_policy_edit(&diff(&[near]))
                .unwrap_or_else(|error| panic!("`{near}` is not a protected path: {error}"));
        }
    }

    /// The headline case, and the reason the boundary exists at all: the lint
    /// configuration is the rule the `clippy` gate judges the attempt by, so an
    /// attempt that edits it has just rewritten its own marks.
    #[test]
    fn an_attempt_editing_the_lint_configuration_is_refused() {
        let (detail, paths) = refusal(&["clippy.toml"]);
        assert_eq!(paths, diff(&["clippy.toml"]));
        assert!(
            detail.contains("judged by"),
            "the refusal has to say which rule was broken, not merely that one was: {detail}",
        );
    }

    /// Editing the script the gates run is the same violation as editing a
    /// configuration file they read, and is refused with the same sentence: a
    /// task that could move the goalposts one way and not the other would learn
    /// to prefer the way that was left open.
    #[test]
    fn editing_the_gate_script_and_the_gate_configuration_refuse_identically() {
        let (script, _) = refusal(&["scripts/quality.sh"]);
        let (configuration, _) = refusal(&["rustfmt.toml"]);
        assert_eq!(
            script, configuration,
            "one rule about the rules needs one sentence, whichever protected path was touched"
        );
    }

    /// Every entry of the protected set is exercised, at its own name and below
    /// it, so no entry survives a mutation that drops it. A directory is
    /// guarded at the repository root and a file by its name at any depth,
    /// because `clippy` and `rustfmt` read the nearest configuration walking up
    /// from the file they are checking — a `clippy.toml` inside a crate is read
    /// by that crate's lint gate.
    #[test]
    fn every_protected_entry_refuses_its_own_path_and_everything_below_it() {
        for entry in &PROTECTED_PATHS {
            let name = entry.name;
            refusal(&[name]);
            match entry.kind {
                ProtectedKind::Directory => {
                    refusal(&[&format!("{name}/inner/gate.sh")]);
                    check_no_policy_edit(&diff(&[&format!("outer/{name}/gate.sh")]))
                        .unwrap_or_else(|error| panic!("`outer/{name}` is not `{name}`: {error}"));
                }
                ProtectedKind::File => {
                    refusal(&[&format!("crates/ktask-core/{name}")]);
                    check_no_policy_edit(&diff(&[&format!("{name}.bak")]))
                        .unwrap_or_else(|error| panic!("`{name}.bak` is not `{name}`: {error}"));
                }
            }
        }
    }

    /// The set itself, pinned. `every_protected_entry_…` walks the table, so
    /// deleting an entry would shorten that test rather than fail it — which is
    /// exactly how a protected path stops being protected without anybody's
    /// attention. The order is asserted with the names because a set that has to
    /// be read in one order every time is a set worth reading in one order.
    #[test]
    fn the_protected_set_is_exactly_the_paths_the_rule_names() {
        let names: Vec<&str> = PROTECTED_PATHS.iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            [
                "scripts",
                ".ktask",
                "clippy.toml",
                "rustfmt.toml",
                ".rustfmt.toml",
                "deny.toml",
                "_typos.toml",
                "typos.toml",
            ]
        );
    }

    /// The spelling is not the point. A path that walks back out of a directory
    /// it never left, or that arrives with the `./` a shell completes, is still
    /// the protected file, and is refused before anything is written. The reason
    /// is asserted too: a refusal that reached the right answer by giving up on
    /// the path is the wrong answer arrived at by the wrong road.
    #[test]
    fn a_spelling_that_walks_backwards_still_refuses_the_same_file() {
        for spelling in [
            "./scripts/quality.sh",
            "scripts/inner/../quality.sh",
            "scripts/./quality.sh",
            ".ktask/./config.toml",
        ] {
            let (detail, paths) = refusal(&[spelling]);
            assert_eq!(
                paths,
                diff(&[spelling]),
                "the refusal quotes the path as it arrived"
            );
            assert!(
                detail.contains("judged by"),
                "`{spelling}` is refused because it is the gate script, not because \
                 the check could not read it: {detail}",
            );
        }
    }

    /// A refusal names every path that broke the rule, not the first one it
    /// found, and leaves the ordinary paths out of the list: a human reading the
    /// record has to be sent to each file that was touched, and not to the ones
    /// that were not.
    #[test]
    fn every_offending_path_is_named_and_the_ordinary_ones_are_left_out() {
        let (_, paths) = refusal(&[
            "crates/ktask-core/src/remediate.rs",
            "clippy.toml",
            "scripts/quality.sh",
            "docs/adr/0067-a-self-healing-boundary-is-a-path-set.md",
            ".ktask/config.toml",
        ]);
        assert_eq!(
            paths,
            diff(&["clippy.toml", "scripts/quality.sh", ".ktask/config.toml"])
        );
    }

    /// A path this check cannot place inside the repository is refused rather
    /// than trusted: an absolute path, or one that climbs out of the checkout,
    /// cannot be compared with the protected set, and the attempt that hands one
    /// over is the attempt that is about to edit something the check cannot see.
    #[test]
    fn a_path_that_cannot_be_located_in_the_repository_is_refused() {
        for escaped in [
            "/home/agent/checkout/scripts/quality.sh",
            "../other-repo/scripts/quality.sh",
            ".",
            "./",
        ] {
            let (detail, paths) = refusal(&[escaped]);
            assert_eq!(paths, diff(&[escaped]));
            assert!(
                detail.contains("cannot be located"),
                "the refusal says why a path it cannot place is refused: {detail}",
            );
        }
    }

    /// The two refusals an unlocatable path can carry arrive together rather than
    /// one hiding the other: `../repo/clippy.toml` climbs out of the checkout and
    /// names a protected file, and a human reading one line needs both facts.
    #[test]
    fn a_path_outside_the_repository_that_names_a_protected_file_says_both() {
        let (detail, paths) = refusal(&["../repo/clippy.toml"]);
        assert!(detail.contains("cannot be located"), "{detail}");
        assert!(detail.contains("judged by"), "{detail}");
        assert_eq!(paths, diff(&["../repo/clippy.toml"]));
    }

    /// The first attempt, before anything has failed and so before anything
    /// could be called a remediation: the refusal is journalled as the task's
    /// failure, classed the way VISION.md §7 classes a forbidden file, and the
    /// projection lands on `Failed` rather than on a phase that carries on.
    #[test]
    fn a_first_attempt_touching_a_protected_path_is_refused_and_journaled() {
        let (class, detail) = journalled(&["clippy.toml"]);
        assert_eq!(class, FailureClass::PolicyFailure);
        assert!(
            detail.contains("clippy.toml"),
            "the record has to name what was touched: {detail}",
        );

        let projected = apply(
            &TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
            &policy_edit_event(&diff(&["clippy.toml"])),
        )
        .expect("a first attempt can be refused for what it touched");
        assert!(
            matches!(
                projected,
                TaskState::Failed {
                    class: FailureClass::PolicyFailure,
                    ..
                }
            ),
            "a refusal ends the task rather than moving it to its next phase: {projected:?}",
        );
    }

    /// A remediation touching the same kind of path is refused the same way and
    /// journalled the same way, which is the whole point of the boundary: the
    /// attempt that exists because a gate refused is the attempt most motivated
    /// to edit that gate, and it is judged by the identical rule.
    #[test]
    fn a_remediation_touching_a_protected_path_is_refused_and_journaled() {
        let (class, detail) = journalled(&["scripts/quality.sh"]);
        assert_eq!(class, FailureClass::PolicyFailure);
        assert!(
            detail.contains("scripts/quality.sh"),
            "the record has to name what was touched: {detail}",
        );

        let projected = apply(
            &TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            },
            &policy_edit_event(&diff(&["scripts/quality.sh"])),
        )
        .expect("a remediation can be refused for what it touched");
        assert!(
            matches!(
                projected,
                TaskState::Failed {
                    class: FailureClass::PolicyFailure,
                    ..
                }
            ),
            "a refusal ends a remediation on the spot: {projected:?}",
        );
    }
}

//! The built-in `dummy` provider: the scenario format it is written in, and the
//! replay of it.
//!
//! VISION.md §12 makes `dummy` a first-class adapter whose responses are
//! "predefined, deterministic … on cue", and §15 makes it the thing the
//! scenario suite, CI and offline development of ktask itself are driven by.
//! Both sentences are about a *file*: if the responses live in code, then every
//! end-to-end case is a code change, and the deterministic part of "deterministic
//! provider" is an agent's promise rather than an artifact an operator can read.
//!
//! So a scenario is TOML, read from the `dummy_scenario_path` setting
//! ([`crate::Config::dummy_scenario_path`]), and this module owns what may be
//! written in it: a list of [`Step`]s, each declaring which session it answers,
//! what that session does, and what it leaves behind. [`Scenario::load`] reads
//! one and refuses anything that could not replay as written.
//!
//! Replay lives beside the format, in [`Dummy`]: a session takes the next step,
//! does what that step declared, and answers the way it was scripted to. The two
//! halves still answer to different tests — the format to what a human may write
//! down, the replay to what a session leaves on disk and on the wire — but one
//! file owns both, because "the scenario cannot mean that" and "the scenario did
//! not do that" are the same finding arrived at from two ends.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use time::OffsetDateTime;

use super::{Capabilities, Invocation, Outcome, Provider};
use crate::{AttemptId, Bus, Error, Event, EventKind, EventSeq, Result, Stream, TaskId};

/// What one scripted session declares it does.
///
/// The five words are VISION.md §12's list of what the `dummy` adapter replays,
/// and they are the whole vocabulary: a step is not free to script a sixth
/// response, because a response the runner has no rule for is a scenario that
/// silently tests nothing. The word is the declaration; the exit status a
/// session reports is a separate field precisely because VISION.md §3's fourth
/// invariant forbids reading a session's success off its exit code, and a
/// scenario has to be able to stage that contradiction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// The session finished and reported it. The ordinary case, and the one a
    /// queue of tasks mostly consists of.
    Success,
    /// The session failed. What kind of failure is decided downstream — by
    /// [`crate::FailureClass`] over what the session printed — rather than here,
    /// so a scenario declares that it failed and leaves the classification to
    /// the code under test.
    Failure,
    /// The session never answers. This is the one outcome whose point is that no
    /// `Outcome` arrives: it is how a scenario proves the attempt watchdog fires
    /// and that an interrupted run recovers to a known state.
    Hang,
    /// The session reported a rate limit rather than doing the work, naming when
    /// it clears in its own output. VISION.md §12 normalizes this into the
    /// `waiting_limit` pause rather than into a failure and a retry.
    Limit,
    /// The session asked a question it cannot proceed without, in its own
    /// output. This is a `waiting_input` pause, and the pause state is where an
    /// operator reads that text — which is why the text is declared here rather
    /// than generated.
    NeedsInput,
}

impl StepOutcome {
    /// Every outcome word, in the order VISION.md §12 lists the responses.
    ///
    /// The ledger the tests count against: a sixth response, or a renamed word,
    /// moves this array and fails them, because renaming a word breaks every
    /// scenario file written before the rename.
    pub const ALL: [Self; 5] = [
        Self::Success,
        Self::Failure,
        Self::Hang,
        Self::Limit,
        Self::NeedsInput,
    ];

    /// The word an operator writes in a scenario file, and the one this type
    /// prints as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Hang => "hang",
            Self::Limit => "limit",
            Self::NeedsInput => "needs_input",
        }
    }

    /// The exit status a session scripted to this word reports when the step
    /// declared none.
    ///
    /// One rule and one exception: a session that failed exited non-zero, and
    /// every other response leaves a status that says nothing. This is a default
    /// about a script, not a measurement standing in for a missing one —
    /// ADR-0049 is about a figure nobody reported, and a scripted session has no
    /// figure to report until its step says one.
    #[must_use]
    pub const fn implied_exit_code(self) -> i32 {
        match self {
            Self::Failure => 1,
            Self::Success | Self::Hang | Self::Limit | Self::NeedsInput => 0,
        }
    }

    /// The outcome a written word names, or `None` when it names none of them.
    ///
    /// `None` is what makes a load error able to name the step it refused, which
    /// a failed enum deserialization cannot: by the time serde has given up, the
    /// position of the step that carried the word has been forgotten.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == word)
    }
}

impl fmt::Display for StepOutcome {
    /// The word as it is written in a scenario file, which is also what a
    /// [`crate::Error::Config`] calls the outcome it refused.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One scripted session: which sessions it answers, and what it does when one
/// arrives.
///
/// Every optional field is optional in the file and stays `Option` here, because
/// the difference between "declared zero" and "not written" is a fact about the
/// scenario an operator wrote, and a default that erases it cannot be reported
/// back to them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Answer every session of this task. One of two cues: a step must declare
    /// exactly one, which is what [`Scenario::validate`] holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_task: Option<TaskId>,
    /// Answer the attempt with this number, whichever task it belongs to. The
    /// coarse cue is `on_task` and the fine one is this: a scenario that wants
    /// the first attempt of a task to fail and its retry to succeed says so by
    /// attempt, and the steps are consumed in the order they were written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_attempt: Option<AttemptId>,
    /// The declared outcome, as written. [`Step::outcome_kind`] reads it as a
    /// [`StepOutcome`], and a scenario holding a word outside the five is not
    /// loadable — see [`Scenario::validate`].
    pub outcome: String,
    /// Everything the session prints as its work, or `None` for a session that
    /// prints nothing. This is where a `limit` names the moment it clears and
    /// where a `needs_input` asks its question, because both are things a
    /// session says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// The exit status to report, or `None` for the one the outcome word implies
    /// ([`StepOutcome::implied_exit_code`]). Declared beats implied in both
    /// directions: `failure` with `exit_code = 0` is how a scenario stages an
    /// agent that reported success and was not done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// How long the session waits before it answers, in milliseconds, or `None`
    /// for no delay. A scenario that must show a run progressing while it waits
    /// for a limit has to be able to wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// Files to leave in the session's working directory, by path and contents.
    /// A task is done because its checks pass over files the session wrote, so a
    /// scenario that stages no files cannot exercise that. Ordered by path, so a
    /// replayed scenario writes the same files in the same order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
}

impl Step {
    /// The outcome word read as one of the five, or `None` when it is not one.
    #[must_use]
    pub fn outcome_kind(&self) -> Option<StepOutcome> {
        StepOutcome::parse(&self.outcome)
    }

    /// The exit status this step's session reports: the declared one where there
    /// is one, and the one the outcome word implies where there is not.
    ///
    /// A step whose outcome word is none of the five implies nothing, and reports
    /// its declared status or zero. That is unreachable through
    /// [`Scenario::load`], which refuses such a step by naming it.
    #[must_use]
    pub fn reported_exit_code(&self) -> i32 {
        self.exit_code
            .or_else(|| self.outcome_kind().map(StepOutcome::implied_exit_code))
            .unwrap_or(0)
    }

    /// How long this step's session waits before it answers. Absent is no delay:
    /// a scenario that meant to wait says so in milliseconds.
    #[must_use]
    pub fn delay(&self) -> Duration {
        self.delay_ms.map_or(Duration::ZERO, Duration::from_millis)
    }
}

/// The steps one scenario file declares, in the order it declared them.
///
/// A scenario is loaded, never assembled at the point of use: the rules
/// [`Scenario::validate`] holds are what make "the scenario did not say what the
/// run did" impossible rather than unlikely, and they run over a file before a
/// single session is scripted from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// The steps, in the order the file wrote them. Empty is readable and never
    /// loadable: a scenario that scripts nothing proves nothing, and
    /// [`Scenario::validate`] says so rather than running an empty plan.
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Scenario {
    /// Reads the scenario at `path` — the file `dummy_scenario_path` names — and
    /// validates it.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the file cannot be read, and [`Error::Config`] keyed by
    /// the path when its contents are not a scenario, and by the step when a step
    /// breaks one of the rules [`Scenario::validate`] holds.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::read(&text, &path.display().to_string())
    }

    /// Reads a scenario from a TOML document and applies [`Scenario::validate`]
    /// to it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a document that is not a scenario — not TOML, a key
    /// the format does not have — and for a scenario that breaks one of the rules
    /// [`Scenario::validate`] holds.
    pub fn from_toml(document: &str) -> Result<Self> {
        Self::read(document, "scenario")
    }

    /// The document this scenario came from, or would be written as.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when a field holds something TOML cannot write, which
    /// for a scenario means contents that are not text.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).map_err(|error| Error::Config {
            key: "scenario".to_owned(),
            detail: format!("the dummy scenario cannot be written as TOML: {error}"),
        })
    }

    /// Refuses a scenario that could not be replayed as written.
    ///
    /// Four rules, each one a way a file could otherwise mean something other
    /// than what an operator read in it. A scenario needs a step, since an empty
    /// one answers no session at all. A step needs exactly one cue: neither means
    /// a step nobody would ever run, and both mean two rules claiming one step,
    /// with the winner decided by field order. An outcome word has to be one of
    /// the five, since a sixth is a response no runner has a rule for. And a
    /// declared file has to sit below the session's working directory: the
    /// isolation a task's evidence is attributed to (VISION.md §10) is the
    /// boundary a scenario must not be able to write across, and a path reaching
    /// above it would have a run damage a checkout it was never in.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming the first rule broken, keyed to the step that
    /// broke it and quoting what the step said.
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            return Err(Error::Config {
                key: "steps".to_owned(),
                detail: "this scenario has no steps; a scenario that scripts \
                         nothing verifies nothing"
                    .to_owned(),
            });
        }
        for (index, step) in self.steps.iter().enumerate() {
            validate_cue(index, step)?;
            validate_outcome(index, step)?;
            validate_files(index, step)?;
        }
        Ok(())
    }

    /// The shared half of [`Scenario::from_toml`] and [`Scenario::load`], which
    /// differ only in what the operator has to go and edit afterwards.
    ///
    /// `origin` keys a document-level failure: the path when there is one to
    /// name, since a file the run refused to start from has to be findable in the
    /// error that reports it.
    fn read(document: &str, origin: &str) -> Result<Self> {
        let scenario: Self = toml::from_str(document).map_err(|error| Error::Config {
            key: origin.to_owned(),
            detail: format!("the dummy scenario is not a readable TOML document: {error}"),
        })?;
        scenario.validate()?;
        Ok(scenario)
    }
}

/// Refuses a step that answers no session, or answers two rules at once.
fn validate_cue(index: usize, step: &Step) -> Result<()> {
    match (step.on_task, step.on_attempt) {
        (Some(_), None) | (None, Some(_)) => Ok(()),
        (None, None) => Err(Error::Config {
            key: step_key(index),
            detail: format!(
                "{} declares neither `on_task` nor `on_attempt`, so no session \
                 would ever run it",
                step_label(index, step)
            ),
        }),
        (Some(task), Some(attempt)) => Err(Error::Config {
            key: step_key(index),
            detail: format!(
                "{} declares both `on_task {task}` and `on_attempt {attempt}`; \
                 a step answers one rule",
                step_label(index, step)
            ),
        }),
    }
}

/// Refuses a step whose outcome word is not one of the five.
fn validate_outcome(index: usize, step: &Step) -> Result<()> {
    if step.outcome_kind().is_some() {
        return Ok(());
    }
    let words: Vec<&str> = StepOutcome::ALL
        .iter()
        .copied()
        .map(StepOutcome::as_str)
        .collect();
    Err(Error::Config {
        key: format!("{}.outcome", step_key(index)),
        detail: format!(
            "{} declares outcome `{}`; a step's outcome is one of {}",
            step_label(index, step),
            step.outcome,
            words.join(", ")
        ),
    })
}

/// Refuses a step that would write outside the session's working directory.
fn validate_files(index: usize, step: &Step) -> Result<()> {
    for path in step.files.keys() {
        if !stays_inside(path) {
            return Err(Error::Config {
                key: format!("{}.files", step_key(index)),
                detail: format!(
                    "{} writes `{path}`, which is not below the working \
                     directory the session runs in; a step's files are paths \
                     inside it",
                    step_label(index, step)
                ),
            });
        }
    }
    Ok(())
}

/// Where a step's problem is, in the words an operator greps for: the position
/// in the file, with the offending key after it.
fn step_key(index: usize) -> String {
    format!("steps[{index}]")
}

/// How a step is referred to in a refusal: by the session it answers where the
/// cue is readable, since "the step for task 2" is what an operator can find in
/// an editor without counting array elements.
fn step_label(index: usize, step: &Step) -> String {
    match (step.on_task, step.on_attempt) {
        (Some(task), None) => format!("step {index} (task {task})"),
        (None, Some(attempt)) => format!("step {index} (attempt {attempt})"),
        _ => format!("step {index}"),
    }
}

/// Whether a declared path can only land below the working directory.
///
/// A path is inside when every component is a name: that one rule refuses the
/// absolute path, the `..` that climbs out of the worktree, the empty key, and
/// the `.` that goes nowhere, without this module keeping a list of what else a
/// path could try.
fn stays_inside(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// The name an operator writes in configuration for this adapter, and the name
/// every refusal it makes is filed under.
pub const PROVIDER_NAME: &str = "dummy";

/// How long a `hang` step sleeps before it notices it is still not answering.
///
/// The tick exists so the wait is not one number at the edge of what
/// [`std::thread::sleep`] can be asked for, not to give the wait a term: a `hang`
/// step has no term, which is the entire point of it.
const HANG_TICK: Duration = Duration::from_secs(3_600);

/// The built-in `dummy` provider: a [`Provider`] that replays a [`Scenario`].
///
/// VISION.md §12's "predefined, deterministic … responses" is what this type adds
/// to the format above: one session per step, in the order the file wrote them,
/// each session doing exactly what its step declared and nothing else. The
/// deterministic half of that is a claim about *bytes*: the files a session leaves,
/// the answer it gives, and the payload of every line it published are the
/// scenario's and nothing else's — no instant, counter, or path the file did not
/// name enters them — so the same file replayed twice answers alike, leaves the
/// same files behind, and shows a watching frontend the same lines in the same
/// order. The envelope around a published line is the journal's to stamp
/// (ADR-0016), and where a step's cue named no attempt that envelope attributes
/// the line to this adapter's session count: the one figure outside the file that
/// is allowed to be seen, and named as such. That is what VISION.md §15's
/// end-to-end suite and every crash-recovery case have to hold for a green run to
/// mean anything.
///
/// **A step is chosen by position, not looked up by cue.** An [`Invocation`]
/// carries a prompt, a model and a directory — everything a CLI needs, and none of
/// the identity a step's cue names — so `steps[0]` answers the first session and
/// `steps[1]` the second. A cue is what its author declared that session *was*,
/// and it does the two things a declaration can do: it lets [`Scenario::validate`]
/// refuse a step that answers nothing, or answers two rules at once, and it
/// attributes what the session printed. Ordering the steps against the queue is
/// the runner's, and only the runner's, because only it knows which task and which
/// attempt it is starting; a scenario whose steps are out of queue order replays
/// them out of order, in a file a reviewer can read.
///
/// Every [`Provider`] method takes `&self`, so the cursor is an [`AtomicUsize`]
/// rather than a `&mut`: one adapter serves a whole run — including the review
/// task a different provider was configured for — and two sessions started at once
/// are handed two different steps rather than the same one twice.
#[derive(Debug)]
pub struct Dummy {
    /// The script, already validated: nothing left in a live adapter is something
    /// a session could fail to replay.
    scenario: Scenario,
    /// How many sessions this adapter has been asked to run, which is also the
    /// index of the step the next one takes.
    sessions: AtomicUsize,
}

impl Dummy {
    /// The provider that replays `scenario`, starting at its first step.
    ///
    /// Validation happens here, so a script assembled in code is refused by the
    /// same rules, in the same words, as one read from a file. A [`Dummy`] that
    /// exists has nothing left in it that could fail to replay, which is what
    /// lets [`Provider::invoke`] run a step without re-checking it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] as [`Scenario::validate`] gives it: keyed to the step that
    /// broke a rule, quoting what that step said.
    pub fn new(scenario: Scenario) -> Result<Self> {
        scenario.validate()?;
        Ok(Self {
            scenario,
            sessions: AtomicUsize::new(0),
        })
    }

    /// The provider for the scenario at `path` — the file the
    /// `dummy_scenario_path` setting names — read, validated, and ready to replay
    /// from its first step.
    ///
    /// # Errors
    ///
    /// As [`Scenario::load`]: [`Error::Io`] when the file cannot be read, and
    /// [`Error::Config`] keyed by the path or by the step when its contents are not
    /// a scenario that can be replayed as written.
    pub fn load(path: &Path) -> Result<Self> {
        Self::new(Scenario::load(path)?)
    }

    /// The refusal for a session that asked for a step the scenario does not have.
    ///
    /// Running out is the most common mistake a scenario makes — three sessions,
    /// two steps — and the answer has to name both numbers, because "the provider
    /// failed" would send an operator to the CLI when the file is what is short.
    fn out_of_steps(&self, session: usize) -> Error {
        let declared = self.scenario.steps.len();
        refused(format!(
            "session {} has no step to replay: this scenario declares {declared} {}, \
             and one step answers one session",
            session.saturating_add(1),
            if declared == 1 { "step" } else { "steps" },
        ))
    }
}

impl Provider for Dummy {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> Capabilities {
        // All three are `false`, and for one reason repeated: a capability is a
        // promise about what an adapter may be *asked* for, and a scenario can be
        // asked for none of these. It prints the text a step wrote rather than a
        // document, so nothing structured is coming; no field of a step names a
        // model, so an id an [`Invocation`] carried would be dropped in silence;
        // and nothing measures a scripted session. VISION.md §12 rejects a
        // configured-vs-reported model mismatch rather than tolerating it, and a
        // `false` is the only answer that lets a caller refuse rather than guess —
        // where it refuses is preflight's and the provider factory's decision.
        Capabilities {
            structured_output: false,
            model_selection: false,
            usage_telemetry: false,
        }
    }

    fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
        // Relaxed because what the cursor owes a caller is a distinct index and
        // nothing else: the script it indexes was fixed before this adapter was
        // built, so no other write has to be visible on the strength of this one.
        let session = self.sessions.fetch_add(1, Ordering::Relaxed);
        let step = self
            .scenario
            .steps
            .get(session)
            .ok_or_else(|| self.out_of_steps(session))?;

        // The prompt decides nothing. A response read out of prompt text would
        // make every scenario a function of wording no step declared, and the
        // model id is nowhere in the format either — which is exactly what the
        // `model_selection: false` above exists to let a caller refuse.
        if !step.delay().is_zero() {
            thread::sleep(step.delay());
        }
        write_declared_files(session, step, &inv.working_dir)?;
        publish_output(step, session, bus);

        if step.outcome_kind() == Some(StepOutcome::Hang) {
            // Everything the step declared has already happened; this *is* the
            // response. An `Outcome` here would be the answer a `hang` exists to
            // withhold, and the watchdog's own firing would have nothing to fire
            // into.
            hang();
        }

        Ok(Outcome {
            exit_code: step.reported_exit_code(),
            stdout: step.stdout.clone().unwrap_or_default(),
            // The format declares one stream, and a step's text is on it: there is
            // nothing here to report as a problem, and inventing a distinction
            // between the two would be a claim about a session that printed one.
            stderr: String::new(),
            // `None`, not `Some(Usage::unavailable())`: nothing asked a scripted
            // session what it spent (ADR-0049), and no scenario names a session to
            // disclose an id for.
            usage: None,
            session_id: None,
        })
    }
}

/// Write the files one step declared, in path order, below the directory the
/// session runs in.
///
/// Their paths were refused above the working directory when the scenario was
/// read, so joining them here cannot climb out of it. Their *parents* are the one
/// question ADR-0051 leaves to this function, and it is answered by making them: a
/// declared file in a directory the run has not created is the ordinary case — the
/// first session of a task writes the first file of it — and a replay that required
/// the directory first could not script one.
///
/// Files land before a step answers, which is what lets a `limit` or a
/// `needs_input` pause on a working tree that exists, and a hung session leave
/// behind the state recovery has to have to resolve to.
fn write_declared_files(index: usize, step: &Step, working_dir: &Path) -> Result<()> {
    for (path, contents) in &step.files {
        let target = working_dir.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|reason| {
                refused(format!(
                    "{} could not make the directory holding `{path}`, below `{}`: \
                     {reason}",
                    step_label(index, step),
                    working_dir.display()
                ))
            })?;
        }
        fs::write(&target, contents).map_err(|reason| {
            refused(format!(
                "{} could not write `{path}`, below `{}`: {reason}",
                step_label(index, step),
                working_dir.display()
            ))
        })?;
    }
    Ok(())
}

/// Hand every line a step printed to a frontend that is watching, oldest first —
/// and when nobody is, do nothing at all, which changes nothing else about the
/// call.
///
/// One [`EventKind::AgentOutput`] per line, because that is what the catalog entry
/// says it is — "one line of agent output, kept from the moment it was read" — and
/// because a ring is sized in `output_ring_lines`: a step that printed a hundred
/// lines as one record would spend a hundred lines of budget to display one row.
///
/// The envelope carries the two identities the step declared and the adapter is
/// therefore allowed to know: the task its cue answers, or none where the cue was
/// an attempt; and the attempt its cue named, or — where the file named no
/// attempt — this adapter's own count of the sessions it has run. An
/// [`Invocation`] hands over no identity of either kind, so the record a run
/// *journals* is the one that attributes a line truly, and it is that record, not
/// this copy, whose sequence and instant are read back: a producer hands over
/// sequence zero because the journal assigns the real one as the row is appended,
/// and stamps its own instant (ADR-0016). The instant here is the moment the line
/// was read, which is all a live view can be asked for and nothing a journal row
/// repeats — the payload alone is what a replay is required to reproduce.
fn publish_output(step: &Step, session: usize, bus: Option<&Bus>) {
    let Some(bus) = bus else {
        return;
    };
    let attempt = step.on_attempt.unwrap_or_else(|| {
        AttemptId::new(u32::try_from(session.saturating_add(1)).unwrap_or(u32::MAX))
    });
    for text in declared_lines(step.stdout.as_deref()) {
        bus.publish(Event {
            seq: EventSeq::new(0),
            ts: OffsetDateTime::now_utc(),
            task_id: step.on_task,
            kind: EventKind::AgentOutput {
                attempt,
                stream: Stream::Stdout,
                text,
            },
        });
    }
}

/// The lines a declared `stdout` is, in the order the file wrote them.
///
/// A newline ends a line rather than opening another, so `"a\n"` is one line while
/// `"a\n\n"` is two and the second is empty — the blank line an operator wrote on
/// purpose. Text with no trailing newline is one line too, which is what a session
/// that printed something and never finished looks like.
fn declared_lines(text: Option<&str>) -> impl Iterator<Item = String> + '_ {
    text.into_iter()
        .flat_map(|body| body.split_inclusive('\n'))
        .map(|line| line.trim_end_matches('\n').to_owned())
}

/// A step's answer, for the one step that never gives one.
///
/// The wait has no term by design. VISION.md §12's `hang` exists to prove the
/// attempt watchdog fires and that an interrupted run resolves to a known state,
/// and an adapter that finished a hung session first would delete the thing the
/// step was written to test. What ends a session like this is the run's watchdog,
/// or nothing.
fn hang() -> ! {
    loop {
        thread::sleep(HANG_TICK);
    }
}

/// A refusal in the shape every adapter's refusals take: the provider named, and
/// the reason in the words only this adapter can give.
fn refused(detail: String) -> Error {
    Error::Provider {
        provider: PROVIDER_NAME.to_owned(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::{Scenario, Step, StepOutcome};
    use crate::{AttemptId, Error, TaskId};
    use std::collections::BTreeMap;
    use std::time::Duration;

    /// The files a step declares, as a document writes them.
    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, contents)| ((*path).to_owned(), (*contents).to_owned()))
            .collect()
    }

    /// The smallest legal step: one task answered, one outcome word.
    fn task_step(task: u32, outcome: &str) -> Step {
        Step {
            on_task: Some(TaskId::new(task)),
            on_attempt: None,
            outcome: outcome.to_owned(),
            stdout: None,
            exit_code: None,
            delay_ms: None,
            files: BTreeMap::new(),
        }
    }

    /// The same, keyed on an attempt number instead of a task.
    fn attempt_step(attempt: u32, outcome: &str) -> Step {
        Step {
            on_task: None,
            on_attempt: Some(AttemptId::new(attempt)),
            ..task_step(0, outcome)
        }
    }

    #[test]
    fn dummy_scenario_round_trips() {
        let mut wrote_files = task_step(1, "success");
        wrote_files.stdout = Some("answered at once\n".to_owned());
        wrote_files.files = files(&[("notes.md", "first\n"), ("src/lib.rs", "// touched\n")]);
        let mut waited = attempt_step(2, "failure");
        waited.stdout = Some("it went wrong\n".to_owned());
        waited.exit_code = Some(3);
        waited.delay_ms = Some(250);
        let scenario = Scenario {
            steps: vec![wrote_files, waited],
        };

        let document = scenario.to_toml().expect("a scenario is writable as TOML");
        let read_back =
            Scenario::from_toml(&document).expect("and reads back as the same scenario");
        assert_eq!(
            read_back, scenario,
            "every declared field of every step survived the trip:\n{document}"
        );

        for expected in [
            "on_task = 1",
            "on_attempt = 2",
            "outcome = \"success\"",
            "outcome = \"failure\"",
            "stdout = ",
            "exit_code = 3",
            "delay_ms = 250",
            "notes.md",
            "// touched",
        ] {
            assert!(
                document.contains(expected),
                "the written scenario must say `{expected}` in those words, not \
                 merely carry a value a reader has to guess at: {document}"
            );
        }

        let second_trip = Scenario::from_toml(&document)
            .expect("the same document reads again")
            .to_toml()
            .expect("and writes again");
        assert_eq!(
            second_trip, document,
            "writing a scenario twice writes it once, which is what keeps a \
             scenario file's own diff reviewable"
        );
    }

    #[test]
    fn a_hand_written_scenario_reads_as_every_field_it_declares() {
        let document = r#"
[[steps]]
on_task = 1
outcome = "needs_input"
stdout = "which of the two APIs is meant?"

[[steps]]
on_attempt = 1
outcome = "limit"
stdout = "rate limit reached, resets at 13:40"
delay_ms = 5

[[steps]]
on_task = 2
outcome = "failure"
exit_code = 0

[[steps]]
on_task = 3
outcome = "hang"
delay_ms = 90000

[steps.files]
"src/lib.rs" = "// written by the dummy provider\n"
"#;
        let scenario = Scenario::from_toml(document).expect("the documented shape loads");
        assert_eq!(scenario.steps.len(), 4);

        let first = &scenario.steps[0];
        assert_eq!(first.on_task, Some(TaskId::new(1)));
        assert_eq!(first.on_attempt, None, "a step names one cue, not two");
        assert_eq!(first.outcome_kind(), Some(StepOutcome::NeedsInput));
        assert_eq!(
            first.stdout.as_deref(),
            Some("which of the two APIs is meant?"),
            "the question an input request asks is the step's own text"
        );
        assert_eq!(first.exit_code, None, "nothing declared an exit code");

        let second = &scenario.steps[1];
        assert_eq!(second.on_attempt, Some(AttemptId::new(1)));
        assert_eq!(second.outcome_kind(), Some(StepOutcome::Limit));
        assert_eq!(second.delay(), Duration::from_millis(5));

        let third = &scenario.steps[2];
        assert_eq!(third.outcome_kind(), Some(StepOutcome::Failure));
        assert_eq!(
            third.reported_exit_code(),
            0,
            "a declared exit code is honoured even where it contradicts the \
             outcome word: completion is never read off an exit status"
        );

        let fourth = &scenario.steps[3];
        assert_eq!(fourth.outcome_kind(), Some(StepOutcome::Hang));
        assert_eq!(fourth.delay(), Duration::from_secs(90));
        assert_eq!(
            fourth.files,
            files(&[("src/lib.rs", "// written by the dummy provider\n")]),
            "a step can declare a file it leaves in the working directory"
        );
    }

    #[test]
    fn every_outcome_word_is_the_one_an_operator_writes() {
        let words: Vec<&str> = StepOutcome::ALL
            .iter()
            .copied()
            .map(StepOutcome::as_str)
            .collect();
        assert_eq!(
            words,
            vec!["success", "failure", "hang", "limit", "needs_input"],
            "these five words are the whole vocabulary VISION.md §12 gives the \
             dummy provider; renaming one breaks every scenario written before \
             the rename"
        );
        for word in words {
            let parsed = StepOutcome::parse(word)
                .unwrap_or_else(|| panic!("`{word}` is a declared outcome word"));
            assert_eq!(parsed.as_str(), word, "a word reads back as itself");
            assert_eq!(parsed.to_string(), word, "and prints as the same word");
        }
    }

    #[test]
    fn an_unknown_outcome_is_a_load_error_naming_the_step() {
        let document = r#"
[[steps]]
on_task = 1
outcome = "success"

[[steps]]
on_task = 2
outcome = "explode"
"#;
        let error = Scenario::from_toml(document)
            .expect_err("an outcome nobody defined cannot be replayed");

        let Error::Config { key, detail } = &error else {
            panic!("a scenario that cannot be read is a config error, not {error}");
        };
        assert_eq!(
            key, "steps[1].outcome",
            "the error points at the step that is wrong, which is the \
             difference between a five-second fix and a search"
        );
        assert!(
            detail.contains("explode"),
            "and it repeats the word it refused: {detail}"
        );
        for word in StepOutcome::ALL.iter().copied().map(StepOutcome::as_str) {
            assert!(
                detail.contains(word),
                "the refusal lists the words that would have been accepted, \
                 missing `{word}`: {detail}"
            );
        }
        assert!(
            detail.contains("task 2"),
            "the step is named by its cue as well as by its position: {detail}"
        );
    }

    #[test]
    fn a_refusal_names_the_attempt_a_step_was_answered_by() {
        let error = Scenario::from_toml(r#"steps = [{ on_attempt = 3, outcome = "explode" }]"#)
            .expect_err("an attempt-cued step is refused as firmly as a task-cued one");
        let Error::Config { key, detail } = &error else {
            panic!("a scenario that cannot be read is a config error, not {error}");
        };
        assert_eq!(key, "steps[0].outcome", "{detail}");
        assert!(
            detail.contains("attempt 3"),
            "the refusal says which attempt it refused, so an operator reads the \
             number they wrote rather than counting array elements: {detail}"
        );
        assert!(
            !detail.contains("task "),
            "a step answered by an attempt is not reported as one answered by a \
             task, which would point at a session that never existed: {detail}"
        );
    }

    #[test]
    fn a_step_that_declares_no_cue_is_refused_naming_its_position() {
        let document = r#"steps = [{ outcome = "success" }]"#;
        let error = Scenario::from_toml(document)
            .expect_err("a step that answers no session scripts nothing");

        let Error::Config { key, detail } = &error else {
            panic!("a scenario that cannot be read is a config error, not {error}");
        };
        assert_eq!(
            key, "steps[0]",
            "the step is named by where it is: {detail}"
        );
        assert!(
            detail.contains("on_task") && detail.contains("on_attempt"),
            "the refusal says which keys it wanted: {detail}"
        );
    }

    #[test]
    fn a_step_that_declares_two_cues_is_refused_naming_its_position() {
        let document = r#"
[[steps]]
on_task = 1
on_attempt = 1
outcome = "success"
"#;
        let error = Scenario::from_toml(document)
            .expect_err("two cues ask one step to answer two different rules");

        let Error::Config { key, detail } = &error else {
            panic!("a scenario that cannot be read is a config error, not {error}");
        };
        assert_eq!(
            key, "steps[0]",
            "the step is named by where it is: {detail}"
        );
        assert!(
            detail.contains("one"),
            "and the refusal names the rule it broke: {detail}"
        );
    }

    #[test]
    fn a_file_below_the_working_directory_is_the_only_kind_a_step_may_write() {
        for document in [
            r#"steps = [{ on_task = 1, outcome = "success", files = { "/etc/passwd" = "x" } }]"#,
            r#"steps = [{ on_task = 1, outcome = "success", files = { "../escape" = "x" } }]"#,
            r#"steps = [{ on_task = 1, outcome = "success", files = { "src/../../escape" = "x" } }]"#,
            r#"steps = [{ on_task = 1, outcome = "success", files = { "" = "x" } }]"#,
        ] {
            let error = Scenario::from_toml(document)
                .expect_err("a scenario cannot be allowed to write outside the task's worktree");
            let Error::Config { key, detail } = &error else {
                panic!("a scenario that cannot be read is a config error, not {error}");
            };
            assert_eq!(key, "steps[0].files", "{document}");
            assert!(
                detail.contains("working directory"),
                "the refusal names the boundary it protects: {detail}"
            );
        }

        let allowed = Scenario::from_toml(
            r#"steps = [{ on_task = 1, outcome = "success", files = { "src/deep/down.rs" = "x" } }]"#,
        )
        .expect("a path below the working directory is what the field is for");
        assert_eq!(
            allowed.steps[0]
                .files
                .get("src/deep/down.rs")
                .map(String::as_str),
            Some("x"),
            "and it arrives unchanged, not normalized into something else"
        );
    }

    #[test]
    fn a_scenario_with_no_steps_is_refused() {
        let error = Scenario::from_toml("steps = []")
            .expect_err("a scenario that replays nothing proves nothing");
        let Error::Config { key, detail } = &error else {
            panic!("a scenario that cannot be read is a config error, not {error}");
        };
        assert_eq!(key, "steps");
        assert!(detail.contains("no steps"), "{detail}");

        let assembled = Scenario::default();
        assert!(
            assembled.validate().is_err(),
            "the rule holds for a scenario built in code too, not only for one read from a file"
        );
    }

    #[test]
    fn a_key_the_format_does_not_define_is_refused() {
        for document in [
            r#"steps = [{ on_task = 1, outcome = "success", timeout_ms = 5 }]"#,
            r#"step = [{ on_task = 1, outcome = "success" }]"#,
            r#"steps = [{ on_task = 1, outcome = "success", on_attempt = 2 }]"#,
        ] {
            let error = Scenario::from_toml(document)
                .expect_err("a key nobody defined must not be read and ignored");
            assert!(
                matches!(error, Error::Config { .. }),
                "and it arrives as a config error, not {error}"
            );
        }
    }

    #[test]
    fn an_absent_exit_code_is_the_one_the_outcome_word_implies() {
        for (word, implied) in [
            ("success", 0),
            ("limit", 0),
            ("needs_input", 0),
            ("hang", 0),
            ("failure", 1),
        ] {
            let step = task_step(1, word);
            assert_eq!(step.exit_code, None, "`{word}` declared no exit code");
            assert_eq!(
                step.reported_exit_code(),
                implied,
                "a session scripted to `{word}` reports {implied} when the file \
                 did not say otherwise"
            );
        }

        let mut declared = task_step(1, "success");
        declared.exit_code = Some(9);
        assert_eq!(
            declared.reported_exit_code(),
            9,
            "what the file says outranks what the word implies"
        );
    }

    #[test]
    fn an_absent_delay_is_no_delay_and_a_declared_one_is_exactly_that_long() {
        assert_eq!(task_step(1, "success").delay(), Duration::ZERO);
        let mut waited = task_step(1, "success");
        waited.delay_ms = Some(1500);
        assert_eq!(waited.delay(), Duration::from_millis(1500));
    }

    #[test]
    fn a_scenario_is_read_from_the_file_its_configuration_names() {
        let directory = tempfile::tempdir().expect("a scratch directory to read from");
        let path = directory.path().join("scenario.toml");
        std::fs::write(
            &path,
            r#"steps = [{ on_task = 1, outcome = "success", stdout = "done\n" }]"#,
        )
        .expect("and the document is writable");

        let loaded = Scenario::load(&path).expect("the file at that path loads");
        assert_eq!(loaded.steps.len(), 1);
        assert_eq!(loaded.steps[0].stdout.as_deref(), Some("done\n"));
        assert_eq!(
            loaded.steps[0].reported_exit_code(),
            0,
            "the defaults apply to a file read from disk exactly as to a document"
        );

        let missing = Scenario::load(&directory.path().join("nowhere.toml"))
            .expect_err("and a path holding nothing is a failure, not an empty scenario");
        assert!(
            matches!(missing, Error::Io(..)),
            "the OS reason is the useful part of this answer, not {missing}"
        );

        std::fs::write(&path, "this is not toml =").expect("a broken document is writable");
        let broken = Scenario::load(&path).expect_err("and reads as a failure");
        let Error::Config { key, detail } = &broken else {
            panic!("a broken scenario file is a config error, not {broken}");
        };
        assert!(
            key.ends_with("scenario.toml"),
            "the error names the file an operator has to go and fix, got {key}"
        );
        assert!(detail.contains("TOML"), "{detail}");
    }

    #[test]
    fn the_files_of_a_step_are_written_in_one_stable_order() {
        let mut step = task_step(1, "success");
        step.files = files(&[("zeta.rs", "z"), ("alpha.rs", "a"), ("mid.rs", "m")]);
        let document = Scenario { steps: vec![step] }
            .to_toml()
            .expect("a scenario with files is writable");

        let alpha = document
            .find("alpha.rs")
            .expect("the declared path is in the document");
        let mid = document.find("mid.rs").expect("and so is the next one");
        let zeta = document.find("zeta.rs").expect("and the last");
        assert!(
            alpha < mid && mid < zeta,
            "files are written in path order, so the same scenario written \
             twice is the same bytes: {document}"
        );
    }

    #[test]
    fn a_step_names_the_session_it_answers() {
        assert_eq!(
            task_step(3, "success").on_task,
            Some(TaskId::new(3)),
            "a task cue answers every session of that task"
        );
        assert_eq!(
            attempt_step(2, "success").on_attempt,
            Some(AttemptId::new(2)),
            "an attempt cue answers one numbered attempt"
        );
    }
}

#[cfg(test)]
mod replay_tests {
    // The other half of this file: what a step *does* when a session runs it.
    // `tests` above never runs a session; every test here runs at least one, and
    // each asserts something a caller, a filesystem, or a watching frontend could
    // check for itself — the bytes on disk, the bytes a bus carried, the status a
    // session reported, or the fact that nothing arrived.

    use super::{Capabilities, Dummy, Invocation, Outcome, Provider, Scenario, Step};
    use crate::{AttemptId, Bus, Error, Event, EventKind, Stream, TaskId};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// How long the hang test waits for an answer that must not come: two orders
    /// of magnitude more than everything a step does before it stops answering.
    const HANG_WINDOW: Duration = Duration::from_millis(500);

    /// A step that prints `text` and nothing else.
    fn labelled(task: u32, text: &str) -> Step {
        Step {
            stdout: Some(text.to_owned()),
            ..task_step(task, "success")
        }
    }

    /// The smallest legal step: one task answered, one outcome word.
    fn task_step(task: u32, outcome: &str) -> Step {
        Step {
            on_task: Some(TaskId::new(task)),
            on_attempt: None,
            outcome: outcome.to_owned(),
            stdout: None,
            exit_code: None,
            delay_ms: None,
            files: BTreeMap::new(),
        }
    }

    /// The same, keyed on an attempt number instead of a task.
    fn attempt_step(attempt: u32, outcome: &str) -> Step {
        Step {
            on_task: None,
            on_attempt: Some(AttemptId::new(attempt)),
            ..task_step(0, outcome)
        }
    }

    /// The files a step declares, as a document writes them.
    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, contents)| ((*path).to_owned(), (*contents).to_owned()))
            .collect()
    }

    /// A session's worth of work, asked of a provider by a caller that knows the
    /// prompt, the model and the directory — and nothing of the scenario.
    fn invocation(working_dir: &Path) -> Invocation {
        Invocation {
            prompt: "implement T056 and run the gates".to_owned(),
            model: None,
            working_dir: working_dir.to_path_buf(),
        }
    }

    /// A provider over these steps, in a scratch directory nobody else touches.
    fn replaying(steps: Vec<Step>) -> (Dummy, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("a scratch worktree to replay into");
        let dummy = Dummy::new(Scenario { steps }).expect("these steps are a legal scenario");
        (dummy, directory)
    }

    /// Every file below `root`, keyed by its path relative to `root` and holding
    /// its exact bytes: the whole reach of a replay, read back off the disk.
    fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut entries = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("the working directory is readable") {
                let path = entry.expect("the directory entry is readable").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path.strip_prefix(root).expect("the path is below the root");
                    entries.insert(
                        relative.to_path_buf(),
                        fs::read(&path).expect("a file reads"),
                    );
                }
            }
        }
        entries
    }

    /// The line a bus event carries, refusing any other catalog entry.
    fn agent_line(event: &Event) -> (Option<TaskId>, AttemptId, Stream, String) {
        let EventKind::AgentOutput {
            attempt,
            stream,
            text,
        } = event.kind.clone()
        else {
            panic!(
                "what a session printed is an AgentOutput, not {:?}",
                event.kind
            );
        };
        (event.task_id, attempt, stream, text)
    }

    /// Run `sessions` sessions of the scenario `document` scripts, each in a fresh
    /// directory with a watching bus, and keep everything the scenario controlled:
    /// what it answered, the files it left, and the exact payload bytes a journal
    /// row would have held for every line it printed.
    type Replayed = (Vec<Outcome>, BTreeMap<PathBuf, Vec<u8>>, Vec<Vec<u8>>);

    fn replay(document: &str, sessions: usize) -> Replayed {
        let scenario = Scenario::from_toml(document).expect("the scenario under test is loadable");
        let directory = tempfile::tempdir().expect("a scratch worktree to replay into");
        let bus = Bus::new();
        let mut viewer = bus.subscribe();
        let provider = Dummy::new(scenario).expect("and replayable");

        let mut outcomes = Vec::new();
        let mut payloads = Vec::new();
        for session in 1..=sessions {
            outcomes.push(
                provider
                    .invoke(&invocation(directory.path()), Some(&bus))
                    .unwrap_or_else(|error| {
                        panic!("step {session} was scripted, and refused: {error}")
                    }),
            );
            let (events, dropped) = viewer.drain();
            assert_eq!(
                dropped, 0,
                "a scenario's own output cannot overflow a live view"
            );
            payloads.extend(
                events
                    .iter()
                    .map(|event| serde_json::to_vec(&event.kind).expect("a catalog entry encodes")),
            );
        }
        (outcomes, tree(directory.path()), payloads)
    }

    /// The whole of the `Provider` interface, as a run holds it: a box, a name,
    /// and a capabilities answer a caller is allowed to ask of.
    #[test]
    fn the_dummy_adapter_names_itself_and_claims_nothing_a_scenario_cannot_do() {
        let (dummy, _directory) = replaying(vec![task_step(1, "success")]);
        let provider: Box<dyn Provider> = Box::new(dummy);

        assert_eq!(provider.name(), "dummy", "the word an operator writes");
        assert_eq!(
            provider.capabilities(),
            Capabilities {
                structured_output: false,
                model_selection: false,
                usage_telemetry: false,
            },
            "a scenario declares text, names no model, and measures nothing: a \
             capability the format cannot honour is the mismatch VISION.md §12 says \
             to reject rather than tolerate"
        );
    }

    #[test]
    fn a_scenario_answers_one_session_per_step_in_the_order_the_file_wrote_them() {
        let (provider, directory) = replaying(vec![
            labelled(1, "first\n"),
            labelled(2, "second\n"),
            labelled(3, "third\n"),
        ]);

        let mut answers = Vec::new();
        for session in 1..=3 {
            let outcome = provider
                .invoke(&invocation(directory.path()), None)
                .unwrap_or_else(|error| {
                    panic!("the scenario scripts a session {session}, and refused: {error}")
                });
            answers.push(outcome.stdout);
        }
        assert_eq!(
            answers,
            vec!["first\n", "second\n", "third\n"],
            "the second session gets the second step, not the first step again"
        );
        assert_eq!(
            tree(directory.path()),
            BTreeMap::new(),
            "three sessions that declared no files wrote nothing"
        );
    }

    #[test]
    fn a_session_past_the_last_step_is_refused_naming_the_scenario_that_ran_out() {
        let (provider, directory) = replaying(vec![labelled(1, "only\n")]);
        provider
            .invoke(&invocation(directory.path()), None)
            .expect("the one step answers the first session");
        let before = tree(directory.path());

        let error = provider
            .invoke(&invocation(directory.path()), None)
            .expect_err("there is no second step to run");
        let Error::Provider {
            provider: name,
            detail,
        } = error
        else {
            panic!("a scenario with nothing left to replay is a provider refusal, not {error}");
        };
        assert_eq!(name, "dummy", "the refusal says who ran out");
        assert!(
            detail.contains("1 step") && detail.contains("session 2"),
            "the refusal has to say how many steps there were and which session \
             asked for one more, got: {detail}"
        );
        assert_eq!(
            tree(directory.path()),
            before,
            "a refused session writes nothing and changes nothing"
        );
        assert!(
            provider
                .invoke(&invocation(directory.path()), None)
                .is_err(),
            "running out is a state, not a one-off: a third ask is refused too"
        );
    }

    #[test]
    fn the_files_a_step_declares_land_below_the_directory_the_session_runs_in() {
        let mut step = task_step(1, "success");
        step.files = files(&[("notes.md", "first\n"), ("src/lib.rs", "// touched\n")]);
        let (provider, directory) = replaying(vec![step]);

        provider
            .invoke(&invocation(directory.path()), None)
            .expect("the session runs");

        assert_eq!(
            tree(directory.path()),
            BTreeMap::from([
                (PathBuf::from("notes.md"), b"first\n".to_vec()),
                (PathBuf::from("src/lib.rs"), b"// touched\n".to_vec()),
            ]),
            "both files arrived, byte for byte, and the directory the session was \
             given is the whole of their reach — including the `src` parent the \
             file's path asked for and the scratch directory never had"
        );
    }

    #[test]
    fn a_later_step_rewrites_a_path_an_earlier_one_wrote() {
        let mut first = task_step(1, "success");
        first.files = files(&[("src/lib.rs", "// half of it\n")]);
        let mut second = task_step(1, "success");
        second.files = files(&[("src/lib.rs", "// all of it\n")]);
        let (provider, directory) = replaying(vec![first, second]);

        provider
            .invoke(&invocation(directory.path()), None)
            .expect("the first session runs");
        provider
            .invoke(&invocation(directory.path()), None)
            .expect("and so does the second");

        assert_eq!(
            tree(directory.path()),
            BTreeMap::from([(PathBuf::from("src/lib.rs"), b"// all of it\n".to_vec())]),
            "a second session editing a file is the ordinary case, and a step that \
             could not rewrite one could not script a retry"
        );
    }

    #[test]
    fn a_file_that_cannot_land_is_refused_naming_the_step_and_the_path() {
        // `blocked` is a plain file in the worktree, so the directory this step's
        // path asks for cannot exist: the write cannot succeed, and it must not
        // pretend to have.
        let mut step = task_step(1, "success");
        step.stdout = Some("past the files\n".to_owned());
        step.files = files(&[("blocked/notes.md", "never\n")]);
        let (provider, directory) = replaying(vec![step]);
        fs::write(directory.path().join("blocked"), b"in the way\n")
            .expect("a file to stand where the directory belongs");
        let bus = Bus::new();
        let mut viewer = bus.subscribe();

        let error = provider
            .invoke(&invocation(directory.path()), Some(&bus))
            .expect_err("a declared file that cannot be written cannot be replayed");

        let Error::Provider {
            provider: name,
            detail,
        } = error
        else {
            panic!("a step that could not write its file is a provider refusal, not {error}");
        };
        assert_eq!(name, "dummy", "the refusal says who could not write");
        assert!(
            detail.contains("blocked/notes.md") && detail.contains("task 1"),
            "the refusal has to name the step, the path, and the reason the OS \
             gave, got: {detail}"
        );
        assert_eq!(
            tree(directory.path()),
            BTreeMap::from([(PathBuf::from("blocked"), b"in the way\n".to_vec())]),
            "a refused write leaves the worktree exactly as it found it — no \
             half-written file for the next session to trip over"
        );
        let (events, dropped) = viewer.drain();
        assert!(
            events.is_empty() && dropped == 0,
            "the step's files come before its output, so a session that never got \
             its file written showed a viewer nothing either ({} events, \
             {dropped} dropped)",
            events.len()
        );
    }

    #[test]
    fn what_a_step_prints_arrives_as_one_agent_output_per_line() {
        let (provider, directory) = replaying(vec![labelled(3, "one\ntwo\n")]);
        let bus = Bus::new();
        let mut viewer = bus.subscribe();

        let outcome = provider
            .invoke(&invocation(directory.path()), Some(&bus))
            .expect("the session runs");

        assert_eq!(
            outcome.stdout, "one\ntwo\n",
            "the caller gets the declared text whole, exactly as the file wrote it"
        );
        let (events, dropped) = viewer.drain();
        assert_eq!(dropped, 0, "two lines cannot overflow a live view");
        assert_eq!(
            events.iter().map(agent_line).collect::<Vec<_>>(),
            vec![
                (
                    Some(TaskId::new(3)),
                    AttemptId::new(1),
                    Stream::Stdout,
                    "one".to_owned()
                ),
                (
                    Some(TaskId::new(3)),
                    AttemptId::new(1),
                    Stream::Stdout,
                    "two".to_owned()
                ),
            ],
            "a frontend's ring is sized in lines, so a step that printed two lines \
             publishes two records, in order, attributed to the task its step answers"
        );
    }

    #[test]
    fn an_attempt_cued_line_carries_the_attempt_the_step_named_and_no_task() {
        let mut step = attempt_step(5, "success");
        step.stdout = Some("the retry\n".to_owned());
        let (provider, directory) = replaying(vec![step]);
        let bus = Bus::new();
        let mut viewer = bus.subscribe();

        provider
            .invoke(&invocation(directory.path()), Some(&bus))
            .expect("the session runs");

        let (events, _) = viewer.drain();
        assert_eq!(
            events.iter().map(agent_line).collect::<Vec<_>>(),
            vec![(
                None,
                AttemptId::new(5),
                Stream::Stdout,
                "the retry".to_owned()
            )],
            "the step named its attempt, not its task, and the line it printed says \
             exactly that and nothing invented beside it"
        );
    }

    #[test]
    fn a_session_that_prints_nothing_publishes_nothing_and_answers_the_same_alone() {
        let (provider, directory) = replaying(vec![task_step(1, "needs_input")]);
        let bus = Bus::new();
        let mut viewer = bus.subscribe();

        let watched = provider
            .invoke(&invocation(directory.path()), Some(&bus))
            .expect("a session that prints nothing still answers");
        let (events, dropped) = viewer.drain();
        assert!(
            events.is_empty() && dropped == 0,
            "nothing was printed, so a viewer was shown nothing ({} events, \
             {dropped} dropped)",
            events.len()
        );

        let (alone_provider, alone_directory) = replaying(vec![task_step(1, "needs_input")]);
        let alone = alone_provider
            .invoke(&invocation(alone_directory.path()), None)
            .expect("and the same session runs with nobody watching");

        assert_eq!(
            alone, watched,
            "a live view is a listener, not an input: a scenario that replayed \
             differently while someone watched could never be reproduced headlessly"
        );
    }

    #[test]
    fn every_outcome_word_reports_the_status_the_step_declared_or_implied() {
        for (word, status) in [
            ("success", 0),
            ("failure", 1),
            ("limit", 0),
            ("needs_input", 0),
        ] {
            let (provider, directory) = replaying(vec![task_step(1, word)]);
            let outcome = provider
                .invoke(&invocation(directory.path()), None)
                .unwrap_or_else(|error| panic!("`{word}` answers, unlike `hang`: {error}"));
            assert_eq!(
                outcome.exit_code, status,
                "the word `{word}` implies {status}"
            );
        }

        let mut lied = task_step(1, "failure");
        lied.exit_code = Some(0);
        let mut honest = task_step(1, "success");
        honest.exit_code = Some(7);
        let (provider, directory) = replaying(vec![lied, honest]);
        assert_eq!(
            provider
                .invoke(&invocation(directory.path()), None)
                .expect("the session that lied about failing runs")
                .exit_code,
            0,
            "a declared status outranks the implied one, which is how a scenario \
             stages the agent that reported success and was not done"
        );
        assert_eq!(
            provider
                .invoke(&invocation(directory.path()), None)
                .expect("and the session that declared a status of its own runs")
                .exit_code,
            7,
            "and in the other direction too"
        );
    }

    #[test]
    fn a_replayed_session_reports_no_figures_because_nothing_measured_it() {
        let (provider, directory) = replaying(vec![labelled(1, "done\n")]);

        let outcome = provider
            .invoke(&invocation(directory.path()), None)
            .expect("the session runs");

        assert_eq!(
            outcome.usage, None,
            "ADR-0049: a scripted session was never measured, and some(0) would \
             record that it spent nothing"
        );
        assert_eq!(
            outcome.session_id, None,
            "a scenario names no session, and no correctness path may depend on one"
        );
        assert_eq!(
            outcome.stderr, "",
            "the format declares one stream and a step's text is on it, so there is \
             nothing to report as a problem"
        );
    }

    #[test]
    fn a_step_waits_the_milliseconds_its_step_declared_before_it_answers() {
        let mut step = task_step(1, "success");
        step.delay_ms = Some(40);
        let (provider, directory) = replaying(vec![step]);

        let started = Instant::now();
        provider
            .invoke(&invocation(directory.path()), None)
            .expect("the session runs, eventually");

        assert!(
            started.elapsed() >= Duration::from_millis(40),
            "the session answered after {:?}, before the 40 ms its step asked to \
             wait: a run cannot show progress through a pause it skips",
            started.elapsed()
        );
    }

    #[test]
    fn a_step_that_declared_no_delay_answers_without_waiting() {
        let (provider, directory) = replaying(vec![labelled(1, "at once\n")]);

        let started = Instant::now();
        provider
            .invoke(&invocation(directory.path()), None)
            .expect("the session runs");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "absent means no delay, and a session that waited {:?} for a step that \
             asked for none makes every scenario run slow enough to look hung",
            started.elapsed()
        );
    }

    #[test]
    fn a_hang_step_does_not_answer_and_its_watchdog_is_the_only_thing_that_ends_it() {
        let mut step = attempt_step(2, "hang");
        step.stdout = Some("thinking about it\n".to_owned());
        step.files = files(&[("work-in-progress.md", "half written\n")]);
        let dummy =
            Dummy::new(Scenario { steps: vec![step] }).expect("a hang step is a legal step");
        let directory = tempfile::tempdir().expect("a scratch worktree to replay into");
        let bus = Bus::new();
        let mut viewer = bus.subscribe();

        let (sender, receiver) = mpsc::channel();
        let working_dir = directory.path().to_path_buf();
        // A bus clone, not the bus itself: a clone shares the subscriber slots, so
        // the view opened above still sees what the hung session printed.
        let provider_bus = bus.clone();
        // The box is the assertion that a `Dummy` travels into the thread a run
        // watches it from, which is the only way a hung session is survivable.
        let provider: Box<dyn Provider + Send> = Box::new(dummy);
        let never_joined = thread::spawn(move || {
            let answer = provider.invoke(&invocation(&working_dir), Some(&provider_bus));
            sender
                .send(answer)
                .expect("the test is still waiting for this session's answer");
        });

        match receiver.recv_timeout(HANG_WINDOW) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(reason) => panic!("the hung session's thread died instead of hanging: {reason}"),
            Ok(Ok(outcome)) => panic!("a `hang` step answered after {HANG_WINDOW:?}: {outcome:?}"),
            Ok(Err(error)) => panic!("a `hang` step refused after {HANG_WINDOW:?}: {error}"),
        }

        assert_eq!(
            fs::read(directory.path().join("work-in-progress.md"))
                .expect("the file the session wrote before it stopped answering"),
            b"half written\n",
            "a hung session got that far, and recovery has to have that state to \
             resolve — an answer is the only thing it withholds"
        );
        let (events, _) = viewer.drain();
        assert_eq!(
            events.iter().map(agent_line).collect::<Vec<_>>(),
            vec![(
                None,
                AttemptId::new(2),
                Stream::Stdout,
                "thinking about it".to_owned()
            )],
            "a frontend watching the attempt has to see what it printed before it \
             stopped answering, or a hung run looks silent and healthy"
        );
        drop(never_joined);
    }

    #[test]
    fn two_sessions_at_once_are_handed_two_different_steps() {
        fn assert_send_sync<T: Provider + Send + Sync>() {}
        assert_send_sync::<Dummy>();

        let (provider, directory) =
            replaying(vec![labelled(1, "first\n"), labelled(2, "second\n")]);
        let barrier = Barrier::new(2);
        let mut answers = Vec::new();
        thread::scope(|threads| {
            let running: Vec<_> = (0..2)
                .map(|_| {
                    threads.spawn(|| {
                        barrier.wait();
                        provider
                            .invoke(&invocation(directory.path()), None)
                            .expect("each of the two has a step")
                    })
                })
                .collect();
            for session in running {
                answers.push(
                    session
                        .join()
                        .expect("a session thread does not panic")
                        .stdout,
                );
            }
        });
        answers.sort();

        assert_eq!(
            answers,
            vec!["first\n", "second\n"],
            "two concurrent sessions were served two different steps; one step \
             handed out twice replays one session's work as another's"
        );
    }

    #[test]
    fn the_same_scenario_replays_byte_identically_across_two_runs() {
        let document = r#"[[steps]]
on_task = 1
outcome = "success"
stdout = "implemented the thing\n"

[steps.files]
"notes.md" = "first\n"
"src/lib.rs" = "// written by the dummy provider\n"

[[steps]]
on_attempt = 2
outcome = "failure"
stdout = "the check failed\n"
exit_code = 3
delay_ms = 5

[steps.files]
"src/lib.rs" = "// retried\n"

[[steps]]
on_task = 2
outcome = "limit"
stdout = "rate limited until 09:15\n"
exit_code = 0
"#;

        // Replayed from the document the format itself writes, because a run gets
        // its scenario through that door too: a hand-written file and the bytes it
        // writes back are the same script in two spellings.
        let script = Scenario::from_toml(document)
            .expect("the scenario under test is a scenario")
            .to_toml()
            .expect("and is writable");
        let (answered, files, payloads) = replay(&script, 3);
        let (answered_again, files_again, payloads_again) = replay(&script, 3);

        assert_eq!(files, files_again, "the two runs left different files");
        assert_eq!(
            answered, answered_again,
            "the two runs answered differently"
        );
        assert_eq!(
            payloads, payloads_again,
            "the payload bytes a journal row would have held differ between two runs \
             of one scenario — the envelope's sequence and instant are the journal's \
             own, and only those are allowed to differ"
        );
        assert_eq!(
            files.get(Path::new("src/lib.rs")).map(Vec::as_slice),
            Some(b"// retried\n".as_slice()),
            "the last step to write a path is the run's answer for it"
        );
        assert_eq!(
            answered[1].exit_code, 3,
            "the second session ran the step that declared status 3, not the first"
        );
        let shown = payloads
            .iter()
            .map(|payload| String::from_utf8(payload.clone()).expect("a payload is UTF-8 text"))
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "implemented the thing",
            "the check failed",
            "rate limited until 09:15",
        ] {
            assert!(
                shown.contains(expected),
                "a viewer never saw `{expected}`: {shown}"
            );
        }

        assert_eq!(
            Scenario::from_toml(&script)
                .expect("a written scenario reads back")
                .to_toml()
                .expect("and writes again"),
            script,
            "writing a scenario is stable, so the file a run replayed is the file \
             an operator reviewed"
        );
    }

    #[test]
    fn a_scenario_file_is_replayed_by_the_provider_the_configuration_names() {
        let scratch = tempfile::tempdir().expect("a place to write the scenario into");
        let path = scratch.path().join("scenario.toml");
        fs::write(
            &path,
            r#"steps = [{ on_task = 1, outcome = "success", stdout = "from the file\n", files = { "made.rs" = "// by the file\n" } }]"#,
        )
        .expect("and the document is writable");
        let worktree = tempfile::tempdir().expect("a scratch worktree to replay into");

        let provider = Dummy::load(&path).expect("the file at that path is replayable");
        let outcome = provider
            .invoke(&invocation(worktree.path()), None)
            .expect("and the session it scripts runs");

        assert_eq!(outcome.stdout, "from the file\n");
        assert_eq!(
            tree(worktree.path()),
            BTreeMap::from([(PathBuf::from("made.rs"), b"// by the file\n".to_vec())])
        );

        let missing = Dummy::load(&scratch.path().join("nowhere.toml"))
            .expect_err("a path holding nothing is a failure, not an empty provider");
        assert!(
            matches!(missing, Error::Io(..)),
            "the OS reason is the useful half of this answer, not {missing}"
        );
    }

    #[test]
    fn a_scenario_that_could_not_replay_is_refused_before_any_session_runs() {
        let mut misspelled = task_step(1, "expode");
        misspelled.stdout = Some("whatever\n".to_owned());
        let error = Dummy::new(Scenario {
            steps: vec![misspelled],
        })
        .expect_err("a sixth outcome word is not a replayable script");
        let Error::Config { key, detail } = &error else {
            panic!("a bad outcome word is the format's refusal, not {error}");
        };
        assert!(key.ends_with("outcome"), "{key}");
        assert!(detail.contains("expode"), "{detail}");

        let cueless = Step {
            on_task: None,
            on_attempt: None,
            ..task_step(0, "success")
        };
        assert!(
            matches!(
                Dummy::new(Scenario {
                    steps: vec![cueless]
                }),
                Err(Error::Config { .. })
            ),
            "a step no session would ever run is refused the same way, whether the \
             scenario came from a file or from code"
        );
    }
}

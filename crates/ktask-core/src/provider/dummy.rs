//! The scenario format the built-in `dummy` provider replays.
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
//! Replay itself — a [`crate::Provider`] that consumes these steps — is not here.
//! A format and a runner over it answer to different tests: this half is about
//! what a human may write down, and is checked without running a session.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path};
use std::time::Duration;

use crate::{AttemptId, Error, Result, TaskId};

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

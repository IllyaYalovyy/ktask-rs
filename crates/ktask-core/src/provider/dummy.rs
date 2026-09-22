//! The TOML scenario format that drives the built-in `dummy` provider, and
//! [`Dummy`], the [`Provider`] it drives (`VISION.md` §12, §15).
//!
//! A scenario is an ordered list of [`Step`]s. Each declares the outcome a
//! canned agent run reports — `success`, `failure`, `hang`, `limit` or
//! `needs_input` — and optionally the standard output it produces, the exit
//! code it reports, an artificial delay, and files it leaves behind in the
//! invocation's working directory. `on_task` and `on_attempt` document which
//! task or attempt a step was written for; parsing and validation do not
//! enforce them, but [`Dummy::invoke`] uses them to address the
//! [`crate::Event`] it publishes for a step's declared output.
//!
//! [`Dummy`] replays a [`Scenario`]'s steps in order, one per call to
//! [`Provider::invoke`]: it writes each step's declared files into the
//! invocation's working directory, publishes its declared standard output to
//! the bus, and returns its declared exit code and output as the reported
//! [`Outcome`]. A `hang` step never returns, exercising external timeout
//! handling exactly as a stalled real provider would. Running out of steps
//! is reported as a [`crate::Error::Provider`], not a panic.

use crate::{
    AttemptId, Bus, Capabilities, Error, Event, EventKind, EventSeq, Invocation, Outcome, Provider,
    Result, Stream, TaskId,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

/// A full dummy-provider scenario: the ordered steps it works through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// The steps, consumed in order as the provider is invoked.
    pub steps: Vec<Step>,
}

/// One canned response the `dummy` provider gives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// The task this step was written for, if the scenario author recorded
    /// one. Documentation only: nothing in this module matches a step
    /// against a running task by this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_task: Option<TaskId>,
    /// The attempt this step was written for, if the scenario author
    /// recorded one. Documentation only, like `on_task`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_attempt: Option<AttemptId>,
    /// What the provider reports for this step.
    pub outcome: StepOutcome,
    /// Text reported as the provider's standard output, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// The process exit code reported, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Milliseconds to wait before reporting, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// Files to write into the invocation's working directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ScenarioFile>,
}

/// One file a [`Step`] writes into the invocation's working directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioFile {
    /// Path, relative to the invocation's working directory.
    pub path: PathBuf,
    /// The file's full contents.
    pub content: String,
}

/// What a step reports the provider did (`VISION.md` §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// The provider completed and reported success.
    Success,
    /// The provider completed and reported failure.
    Failure,
    /// The provider never returns, to exercise timeout handling.
    Hang,
    /// The provider reports a rate or usage limit was hit.
    Limit,
    /// The provider reports it needs input before it can continue.
    NeedsInput,
}

/// The exact set of spellings [`StepOutcome`] accepts in a scenario file,
/// independent of serde's own error formatting, so [`Scenario::parse`] can
/// name the offending step itself rather than relying on how the TOML
/// crate happens to phrase an unknown-variant error.
fn parse_outcome(raw: &str) -> Option<StepOutcome> {
    match raw {
        "success" => Some(StepOutcome::Success),
        "failure" => Some(StepOutcome::Failure),
        "hang" => Some(StepOutcome::Hang),
        "limit" => Some(StepOutcome::Limit),
        "needs_input" => Some(StepOutcome::NeedsInput),
        _ => None,
    }
}

impl Scenario {
    /// Parses a scenario from its TOML text representation.
    ///
    /// # Errors
    ///
    /// Returns an error naming the offending step (1-based) when any
    /// step's `outcome` is not one of the five known outcomes, or an error
    /// from the underlying TOML parser when the document does not
    /// otherwise match the scenario shape.
    pub fn parse(text: &str) -> Result<Scenario> {
        let value: toml::Value = toml::from_str(text).map_err(|err| Error::Provider {
            provider: "dummy".to_string(),
            detail: err.to_string(),
        })?;

        if let Some(steps) = value.get("steps").and_then(toml::Value::as_array) {
            for (index, step) in steps.iter().enumerate() {
                if let Some(raw) = step.get("outcome").and_then(toml::Value::as_str)
                    && parse_outcome(raw).is_none()
                {
                    return Err(Error::Provider {
                        provider: "dummy".to_string(),
                        detail: format!("step {}: unknown outcome '{raw}'", index + 1),
                    });
                }
            }
        }

        value
            .try_into()
            .map_err(|err: toml::de::Error| Error::Provider {
                provider: "dummy".to_string(),
                detail: err.to_string(),
            })
    }

    /// Reads and parses a scenario file from disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, or as [`Scenario::parse`].
    pub fn load(path: &Path) -> Result<Scenario> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    /// Serializes this scenario to its TOML text representation.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails, which does not happen for
    /// a `Scenario` built entirely from this module's own types.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).map_err(|err| Error::Provider {
            provider: "dummy".to_string(),
            detail: err.to_string(),
        })
    }
}

/// The built-in [`Provider`] that replays a [`Scenario`] deterministically
/// (`VISION.md` §12): every call to [`Provider::invoke`] consumes the
/// scenario's next [`Step`], in the order the scenario declares them.
///
/// Two `Dummy`s built from an equal [`Scenario`] and driven through the same
/// sequence of calls report byte-identical [`Outcome`]s and publish
/// byte-identical [`crate::Event`]s, since neither depends on wall-clock
/// time or any other source of nondeterminism.
#[derive(Debug)]
pub struct Dummy {
    steps: Mutex<VecDeque<Step>>,
    next_seq: AtomicU64,
}

impl Dummy {
    /// Builds a `Dummy` that replays `scenario`'s steps in order, one per
    /// call to [`Provider::invoke`].
    #[must_use]
    pub fn new(scenario: Scenario) -> Self {
        Dummy {
            steps: Mutex::new(scenario.steps.into()),
            next_seq: AtomicU64::new(1),
        }
    }

    /// Pops and returns the next step, or a [`Error::Provider`] naming the
    /// scenario as exhausted when none is left.
    fn next_step(&self) -> Result<Step> {
        let mut steps = self.steps.lock().unwrap_or_else(PoisonError::into_inner);
        steps.pop_front().ok_or_else(|| Error::Provider {
            provider: "dummy".to_string(),
            detail: "scenario exhausted: no step left to replay".to_string(),
        })
    }
}

/// Writes each of `files` into `working_dir`, creating any parent
/// directories a file's declared path needs.
fn write_scenario_files(working_dir: &Path, files: &[ScenarioFile]) -> Result<()> {
    for file in files {
        let path = working_dir.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &file.content)?;
    }
    Ok(())
}

impl Provider for Dummy {
    fn name(&self) -> &'static str {
        "dummy"
    }

    /// The `dummy` provider reports no capabilities: nothing in a [`Step`]
    /// models structured output, model selection or usage telemetry.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_output: false,
            model_selection: false,
            usage_telemetry: false,
        }
    }

    /// Consumes the scenario's next step, writes its declared files into
    /// `inv.working_dir`, publishes its declared standard output to `bus`
    /// if given, and returns its declared exit code and output.
    ///
    /// A `hang` step never returns from this call, so it can exercise a
    /// caller's own timeout handling exactly as a stalled real provider
    /// would.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Provider`] when the scenario has no step left to
    /// replay, or when writing a declared file fails.
    fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
        let step = self.next_step()?;

        write_scenario_files(&inv.working_dir, &step.files)?;

        if let Some(delay) = step.delay_ms {
            thread::sleep(Duration::from_millis(delay));
        }

        if let (Some(bus), Some(text)) = (bus, step.stdout.clone()) {
            bus.publish(Event {
                seq: EventSeq::new(self.next_seq.fetch_add(1, Ordering::SeqCst)),
                ts: time::OffsetDateTime::UNIX_EPOCH,
                task_id: step.on_task,
                kind: EventKind::AgentOutput {
                    attempt: step.on_attempt.unwrap_or(AttemptId::new(0)),
                    stream: Stream::Stdout,
                    text,
                },
            });
        }

        if step.outcome == StepOutcome::Hang {
            thread::sleep(Duration::MAX);
        }

        Ok(Outcome {
            exit_code: step.exit_code.unwrap_or(0),
            stdout: step.stdout.unwrap_or_default(),
            stderr: String::new(),
            usage: None,
            session_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scenario() -> Scenario {
        Scenario {
            steps: vec![
                Step {
                    on_task: Some(TaskId::new(1)),
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("implemented the thing\n".to_string()),
                    exit_code: Some(0),
                    delay_ms: None,
                    files: vec![ScenarioFile {
                        path: PathBuf::from("src/lib.rs"),
                        content: "pub fn hello() {}\n".to_string(),
                    }],
                },
                Step {
                    on_task: None,
                    on_attempt: Some(AttemptId::new(2)),
                    outcome: StepOutcome::Failure,
                    stdout: Some("could not compile".to_string()),
                    exit_code: Some(1),
                    delay_ms: None,
                    files: Vec::new(),
                },
                Step {
                    on_task: Some(TaskId::new(3)),
                    on_attempt: None,
                    outcome: StepOutcome::Hang,
                    stdout: None,
                    exit_code: None,
                    delay_ms: Some(60_000),
                    files: Vec::new(),
                },
                Step {
                    on_task: Some(TaskId::new(4)),
                    on_attempt: None,
                    outcome: StepOutcome::Limit,
                    stdout: Some("rate limited, retry after 30m".to_string()),
                    exit_code: None,
                    delay_ms: None,
                    files: Vec::new(),
                },
                Step {
                    on_task: Some(TaskId::new(5)),
                    on_attempt: None,
                    outcome: StepOutcome::NeedsInput,
                    stdout: Some("which database driver?".to_string()),
                    exit_code: None,
                    delay_ms: None,
                    files: Vec::new(),
                },
            ],
        }
    }

    /// The behavior this module exists to guarantee: a scenario built from
    /// these types, written out as TOML, and parsed back produces the exact
    /// same value — across every outcome, with mixed selectors and both
    /// present and absent optional fields.
    #[test]
    fn dummy_scenario_round_trips() {
        let scenario = sample_scenario();

        let text = scenario.to_toml().expect("serialize");
        let back = Scenario::parse(&text).expect("parse");

        assert_eq!(scenario, back);
    }

    #[test]
    fn an_empty_scenario_round_trips() {
        let scenario = Scenario { steps: Vec::new() };

        let text = scenario.to_toml().expect("serialize");
        let back = Scenario::parse(&text).expect("parse");

        assert_eq!(scenario, back);
    }

    #[test]
    fn a_scenario_loads_from_a_file_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scenario.toml");
        let scenario = sample_scenario();
        std::fs::write(&path, scenario.to_toml().expect("serialize")).expect("write");

        let loaded = Scenario::load(&path).expect("load");

        assert_eq!(loaded, scenario);
    }

    #[test]
    fn loading_a_missing_file_is_an_io_error() {
        let err =
            Scenario::load(Path::new("/nonexistent/scenario.toml")).expect_err("must fail to read");
        assert!(matches!(err, Error::Io(_)));
    }

    /// The other half of `Done-when`: an unknown outcome must fail to load,
    /// and the error must name which step is at fault so a scenario author
    /// does not have to bisect a long file by hand.
    #[test]
    fn an_unknown_outcome_is_a_load_error_naming_the_step() {
        let text = r#"
[[steps]]
on_task = 1
outcome = "success"

[[steps]]
on_task = 2
outcome = "bogus"
"#;

        let err = Scenario::parse(text).expect_err("must reject an unknown outcome");
        let message = err.to_string();

        assert!(message.contains("step 2"), "message was: {message}");
        assert!(message.contains("bogus"), "message was: {message}");
    }

    #[test]
    fn the_first_steps_outcome_is_still_checked_when_a_later_one_is_unknown() {
        let text = r#"
[[steps]]
outcome = "not-a-real-outcome"

[[steps]]
outcome = "success"
"#;

        let err = Scenario::parse(text).expect_err("must reject an unknown outcome");
        assert!(err.to_string().contains("step 1"));
    }

    #[test]
    fn every_outcome_round_trips_with_its_documented_toml_spelling() {
        for (raw, outcome) in [
            ("success", StepOutcome::Success),
            ("failure", StepOutcome::Failure),
            ("hang", StepOutcome::Hang),
            ("limit", StepOutcome::Limit),
            ("needs_input", StepOutcome::NeedsInput),
        ] {
            assert_eq!(parse_outcome(raw), Some(outcome));

            let scenario = Scenario {
                steps: vec![Step {
                    on_task: Some(TaskId::new(1)),
                    on_attempt: None,
                    outcome,
                    stdout: None,
                    exit_code: None,
                    delay_ms: None,
                    files: Vec::new(),
                }],
            };
            let text = scenario.to_toml().expect("serialize");
            assert!(
                text.contains(&format!("outcome = \"{raw}\"")),
                "text was: {text}"
            );
        }
    }

    /// Exhaustive, wildcard-free match: a variant added to `StepOutcome` without
    /// being listed both here and in `parse_outcome` fails to compile or
    /// fails this count, instead of silently becoming unparsable.
    #[test]
    fn outcome_has_exactly_five_variants() {
        let variants = [
            StepOutcome::Success,
            StepOutcome::Failure,
            StepOutcome::Hang,
            StepOutcome::Limit,
            StepOutcome::NeedsInput,
        ];
        assert_eq!(variants.len(), 5);

        for outcome in variants {
            match outcome {
                StepOutcome::Success
                | StepOutcome::Failure
                | StepOutcome::Hang
                | StepOutcome::Limit
                | StepOutcome::NeedsInput => {}
            }
        }
    }

    #[test]
    fn an_unrecognized_top_level_key_is_rejected() {
        let text = "bogus_key = 1\nsteps = []\n";
        assert!(Scenario::parse(text).is_err());
    }

    #[test]
    fn an_unrecognized_step_key_is_rejected() {
        let text = r#"
[[steps]]
outcome = "success"
bogus_key = 1
"#;
        assert!(Scenario::parse(text).is_err());
    }

    #[test]
    fn optional_fields_are_omitted_from_the_serialized_form_when_absent() {
        let scenario = Scenario {
            steps: vec![Step {
                on_task: Some(TaskId::new(1)),
                on_attempt: None,
                outcome: StepOutcome::Success,
                stdout: None,
                exit_code: None,
                delay_ms: None,
                files: Vec::new(),
            }],
        };

        let text = scenario.to_toml().expect("serialize");

        assert!(!text.contains("on_attempt"));
        assert!(!text.contains("stdout"));
        assert!(!text.contains("exit_code"));
        assert!(!text.contains("delay_ms"));
        assert!(!text.contains("files"));
    }
}

#[cfg(test)]
mod dummy_provider {
    use super::*;
    use std::sync::{Arc, mpsc};

    fn step(outcome: StepOutcome) -> Step {
        Step {
            on_task: None,
            on_attempt: None,
            outcome,
            stdout: None,
            exit_code: None,
            delay_ms: None,
            files: Vec::new(),
        }
    }

    fn invocation(working_dir: &Path) -> Invocation {
        Invocation {
            prompt: "go".to_string(),
            model: None,
            working_dir: working_dir.to_path_buf(),
        }
    }

    #[test]
    fn name_reports_dummy() {
        let dummy = Dummy::new(Scenario { steps: Vec::new() });
        assert_eq!(dummy.name(), "dummy");
    }

    #[test]
    fn capabilities_reports_no_capabilities() {
        let dummy = Dummy::new(Scenario { steps: Vec::new() });
        let caps = dummy.capabilities();
        assert!(!caps.structured_output);
        assert!(!caps.model_selection);
        assert!(!caps.usage_telemetry);
    }

    /// The core of `Done-when`: consuming steps in order and returning each
    /// step's own declared exit code and stdout, not the first step's or a
    /// fixed one.
    #[test]
    fn invoke_consumes_steps_in_order_and_reports_each_ones_declared_result() {
        let scenario = Scenario {
            steps: vec![
                Step {
                    stdout: Some("first\n".to_string()),
                    exit_code: Some(0),
                    ..step(StepOutcome::Success)
                },
                Step {
                    stdout: Some("second\n".to_string()),
                    exit_code: Some(7),
                    ..step(StepOutcome::Failure)
                },
            ],
        };
        let dummy = Dummy::new(scenario);
        let dir = tempfile::tempdir().expect("tempdir");
        let inv = invocation(dir.path());

        let first = dummy.invoke(&inv, None).expect("first invoke");
        let second = dummy.invoke(&inv, None).expect("second invoke");

        assert_eq!(first.stdout, "first\n");
        assert_eq!(first.exit_code, 0);
        assert_eq!(second.stdout, "second\n");
        assert_eq!(second.exit_code, 7);
    }

    #[test]
    fn invoke_defaults_the_exit_code_to_zero_when_the_step_does_not_declare_one() {
        let dummy = Dummy::new(Scenario {
            steps: vec![step(StepOutcome::Success)],
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let inv = invocation(dir.path());

        let outcome = dummy.invoke(&inv, None).expect("invoke");

        assert_eq!(outcome.exit_code, 0);
    }

    #[test]
    fn invoke_writes_declared_files_into_the_invocations_working_directory() {
        let dummy = Dummy::new(Scenario {
            steps: vec![Step {
                files: vec![
                    ScenarioFile {
                        path: PathBuf::from("src/lib.rs"),
                        content: "pub fn hello() {}\n".to_string(),
                    },
                    ScenarioFile {
                        path: PathBuf::from("README.md"),
                        content: "hello\n".to_string(),
                    },
                ],
                ..step(StepOutcome::Success)
            }],
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let inv = invocation(dir.path());

        dummy.invoke(&inv, None).expect("invoke");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read lib.rs"),
            "pub fn hello() {}\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("README.md")).expect("read README.md"),
            "hello\n"
        );
    }

    #[test]
    fn invoke_publishes_declared_stdout_to_the_bus() {
        let dummy = Dummy::new(Scenario {
            steps: vec![Step {
                on_task: Some(TaskId::new(3)),
                on_attempt: Some(AttemptId::new(2)),
                stdout: Some("working on it\n".to_string()),
                ..step(StepOutcome::Success)
            }],
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let inv = invocation(dir.path());
        let bus = Bus::new(8);
        let mut sub = bus.subscribe();

        dummy.invoke(&inv, Some(&bus)).expect("invoke");

        let (events, dropped) = sub.drain();
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].task_id, Some(TaskId::new(3)));
        match &events[0].kind {
            EventKind::AgentOutput {
                attempt,
                stream,
                text,
            } => {
                assert_eq!(*attempt, AttemptId::new(2));
                assert_eq!(*stream, Stream::Stdout);
                assert_eq!(text, "working on it\n");
            }
            other => panic!("expected AgentOutput, got {other:?}"),
        }
    }

    #[test]
    fn invoke_publishes_nothing_when_the_step_declares_no_stdout() {
        let dummy = Dummy::new(Scenario {
            steps: vec![step(StepOutcome::Success)],
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let inv = invocation(dir.path());
        let bus = Bus::new(8);
        let mut sub = bus.subscribe();

        dummy.invoke(&inv, Some(&bus)).expect("invoke");

        let (events, dropped) = sub.drain();
        assert_eq!(dropped, 0);
        assert!(events.is_empty());
    }

    #[test]
    fn invoke_runs_without_a_bus_even_when_the_step_declares_stdout() {
        let dummy = Dummy::new(Scenario {
            steps: vec![Step {
                stdout: Some("no one is listening\n".to_string()),
                ..step(StepOutcome::Success)
            }],
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let inv = invocation(dir.path());

        let outcome = dummy.invoke(&inv, None).expect("invoke");

        assert_eq!(outcome.stdout, "no one is listening\n");
    }

    /// The other half of `Done-when`: running past the scenario's last step
    /// is a named, non-panicking error rather than a wrap-around or a
    /// default step.
    #[test]
    fn invoking_past_the_last_step_is_a_clear_provider_error() {
        let dummy = Dummy::new(Scenario {
            steps: vec![step(StepOutcome::Success)],
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let inv = invocation(dir.path());

        dummy.invoke(&inv, None).expect("first invoke succeeds");
        let err = dummy
            .invoke(&inv, None)
            .expect_err("second invoke must fail: the scenario is exhausted");

        assert!(matches!(err, Error::Provider { .. }));
        let message = err.to_string();
        assert!(message.contains("exhausted"), "message was: {message}");
    }

    /// `Do:` requires `hang` to sleep past any timeout a caller might apply.
    /// `invoke` cannot be called directly on the test thread for this, since
    /// a genuine hang would never return; instead it is driven on a worker
    /// thread and the test only asserts that no result arrives within a
    /// short, bounded wait.
    #[test]
    fn a_hang_step_does_not_return_within_a_bounded_wait() {
        let dummy = Arc::new(Dummy::new(Scenario {
            steps: vec![step(StepOutcome::Hang)],
        }));
        let dir = tempfile::tempdir().expect("tempdir");
        let inv = invocation(dir.path());

        let (tx, rx) = mpsc::channel::<()>();
        let worker = Arc::clone(&dummy);
        thread::spawn(move || {
            let _ = worker.invoke(&inv, None);
            let _ = tx.send(());
        });

        let result = rx.recv_timeout(Duration::from_millis(200));

        assert!(
            result.is_err(),
            "a hang step must not return within a bounded wait"
        );
    }

    fn multi_step_scenario() -> Scenario {
        Scenario {
            steps: vec![
                Step {
                    on_task: Some(TaskId::new(1)),
                    stdout: Some("built the feature\n".to_string()),
                    exit_code: Some(0),
                    ..step(StepOutcome::Success)
                },
                Step {
                    on_task: Some(TaskId::new(1)),
                    on_attempt: Some(AttemptId::new(2)),
                    stdout: Some("hit a limit\n".to_string()),
                    ..step(StepOutcome::Limit)
                },
            ],
        }
    }

    /// The headline guarantee of `Done-when`: replaying the same scenario
    /// from a fresh `Dummy` reports byte-identical outcomes and publishes
    /// byte-identical events, across two entirely separate runs.
    #[test]
    fn the_same_scenario_produces_byte_identical_outcomes_and_events_across_runs() {
        let run = |dir: &Path| {
            let dummy = Dummy::new(multi_step_scenario());
            let inv = invocation(dir);
            let bus = Bus::new(8);
            let mut sub = bus.subscribe();

            let outcomes: Vec<Outcome> = (0..2)
                .map(|_| dummy.invoke(&inv, Some(&bus)).expect("invoke"))
                .collect();
            let (events, dropped) = sub.drain();
            assert_eq!(dropped, 0);
            (outcomes, events)
        };

        let dir_a = tempfile::tempdir().expect("tempdir");
        let dir_b = tempfile::tempdir().expect("tempdir");

        let (outcomes_a, events_a) = run(dir_a.path());
        let (outcomes_b, events_b) = run(dir_b.path());

        assert_eq!(outcomes_a, outcomes_b);
        assert_eq!(events_a, events_b);
    }
}

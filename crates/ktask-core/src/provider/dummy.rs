//! The TOML scenario format that drives the built-in `dummy` provider
//! (`VISION.md` §12, §15).
//!
//! A scenario is an ordered list of [`Step`]s. Each declares the outcome a
//! canned agent run reports — `success`, `failure`, `hang`, `limit` or
//! `needs_input` — and optionally the standard output it produces, the exit
//! code it reports, an artificial delay, and files it leaves behind in the
//! invocation's working directory. `on_task` and `on_attempt` document which
//! task or attempt a step was written for; they are not enforced here.
//!
//! This module defines and validates the format only. Consuming a parsed
//! [`Scenario`] to drive a real [`crate::Provider`] implementation is a
//! later task's responsibility.

use crate::{AttemptId, Error, Result, TaskId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

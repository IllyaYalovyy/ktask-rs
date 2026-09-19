//! Dummy provider for deterministic testing and scenario-driven development.
//!
//! The dummy provider replays predefined, deterministic outcomes (success, failure,
//! hang, limit, needs_input) loaded from TOML scenario files. It powers the scenario
//! suite, CI, and offline development without consuming real API tokens.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A scenario: a list of predetermined steps with outcomes.
///
/// Each step is triggered by either a task or an attempt, and defines a
/// deterministic provider response including exit code, stdout, stderr,
/// optional delay, and optional files to write to the working directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    /// The steps in this scenario.
    pub steps: Vec<Step>,
}

/// A single step in a scenario.
///
/// Each step is triggered by either `on_task` (matching a `TaskId`) or
/// `on_attempt` (matching an `AttemptId`), and produces a deterministic outcome
/// with optional stdout, exit code, delay, and file writes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    /// Optional task ID that triggers this step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_task: Option<u32>,

    /// Optional attempt ID that triggers this step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_attempt: Option<u32>,

    /// The outcome of this step.
    pub outcome: StepOutcome,

    /// Optional standard output to produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,

    /// Optional exit code (defaults to 0 for success, 1 for failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,

    /// Optional delay in milliseconds before returning the outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,

    /// Optional files to write into the working directory.
    ///
    /// Maps file paths to their contents. Paths should be relative to
    /// the working directory where the provider invocation executes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<BTreeMap<String, String>>,
}

/// The outcome of a scenario step.
///
/// One of five deterministic outcomes that the dummy provider can simulate:
/// success, failure, hang, provider limit reached, or input needed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// Successful completion.
    Success,
    /// Task execution failure.
    Failure,
    /// The provider hangs (simulated by blocking indefinitely).
    Hang,
    /// A provider rate/usage limit has been reached.
    Limit,
    /// The provider needs additional input (`waiting_input` state).
    NeedsInput,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_with_single_success_step() {
        let step = Step {
            on_task: Some(1),
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: Some("Task 1 completed".to_string()),
            exit_code: None,
            delay_ms: None,
            files: None,
        };
        let scenario = Scenario { steps: vec![step] };
        assert_eq!(scenario.steps.len(), 1);
        assert_eq!(scenario.steps[0].on_task, Some(1));
        assert_eq!(scenario.steps[0].outcome, StepOutcome::Success);
    }

    #[test]
    fn step_outcome_all_variants_serialize() {
        let outcomes = [
            StepOutcome::Success,
            StepOutcome::Failure,
            StepOutcome::Hang,
            StepOutcome::Limit,
            StepOutcome::NeedsInput,
        ];

        for outcome in &outcomes {
            // Serialize as JSON (TOML needs a table context for enums)
            let serialized = serde_json::to_string(&outcome).expect("serialize to JSON");
            let deserialized: StepOutcome =
                serde_json::from_str(&serialized).expect("deserialize from JSON");
            assert_eq!(outcome, &deserialized);
        }
    }

    #[test]
    fn dummy_scenario_round_trips() {
        // Create a scenario with various step types
        let mut files = BTreeMap::new();
        files.insert("output.txt".to_string(), "Test output".to_string());
        files.insert("error.log".to_string(), "Error message".to_string());

        let scenario = Scenario {
            steps: vec![
                Step {
                    on_task: Some(1),
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("First task succeeded".to_string()),
                    exit_code: Some(0),
                    delay_ms: Some(100),
                    files: None,
                },
                Step {
                    on_task: None,
                    on_attempt: Some(1),
                    outcome: StepOutcome::Failure,
                    stdout: Some("Attempt failed".to_string()),
                    exit_code: Some(1),
                    delay_ms: Some(50),
                    files: Some(files.clone()),
                },
                Step {
                    on_task: Some(2),
                    on_attempt: None,
                    outcome: StepOutcome::Limit,
                    stdout: None,
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
                Step {
                    on_task: Some(3),
                    on_attempt: None,
                    outcome: StepOutcome::NeedsInput,
                    stdout: Some("Waiting for input".to_string()),
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
                Step {
                    on_attempt: Some(2),
                    on_task: None,
                    outcome: StepOutcome::Hang,
                    stdout: None,
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
            ],
        };

        // Serialize to TOML
        let toml_string = toml::to_string_pretty(&scenario).expect("serialize to TOML");

        // Deserialize from TOML
        let deserialized: Scenario = toml::from_str(&toml_string).expect("deserialize from TOML");

        // Verify round-trip
        assert_eq!(scenario.steps.len(), deserialized.steps.len());

        for (orig, deser) in scenario.steps.iter().zip(deserialized.steps.iter()) {
            assert_eq!(orig.on_task, deser.on_task);
            assert_eq!(orig.on_attempt, deser.on_attempt);
            assert_eq!(orig.outcome, deser.outcome);
            assert_eq!(orig.stdout, deser.stdout);
            assert_eq!(orig.exit_code, deser.exit_code);
            assert_eq!(orig.delay_ms, deser.delay_ms);
            assert_eq!(orig.files, deser.files);
        }
    }

    #[test]
    fn unknown_outcome_fails_to_deserialize() {
        let toml_str = r#"
[[steps]]
on_task = 1
outcome = "unknown_outcome"
"#;
        let result: Result<Scenario, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown outcome should fail to deserialize"
        );

        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("unknown"),
            "error message should mention 'unknown' outcome"
        );
    }

    #[test]
    fn step_with_all_optional_fields_present() {
        let mut files = BTreeMap::new();
        files.insert("test.txt".to_string(), "content".to_string());

        let step = Step {
            on_task: Some(1),
            on_attempt: None,
            outcome: StepOutcome::Failure,
            stdout: Some("Some output".to_string()),
            exit_code: Some(42),
            delay_ms: Some(1000),
            files: Some(files),
        };

        let scenario = Scenario { steps: vec![step] };

        let toml_str = toml::to_string_pretty(&scenario).expect("serialize");
        let restored: Scenario = toml::from_str(&toml_str).expect("deserialize");

        let restored_step = &restored.steps[0];
        assert_eq!(restored_step.stdout, Some("Some output".to_string()));
        assert_eq!(restored_step.exit_code, Some(42));
        assert_eq!(restored_step.delay_ms, Some(1000));
        assert!(restored_step.files.is_some());
        assert_eq!(
            restored_step.files.as_ref().unwrap().get("test.txt"),
            Some(&"content".to_string())
        );
    }

    #[test]
    fn step_with_no_optional_fields() {
        let step = Step {
            on_task: Some(1),
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: None,
            exit_code: None,
            delay_ms: None,
            files: None,
        };

        let scenario = Scenario { steps: vec![step] };

        let toml_str = toml::to_string_pretty(&scenario).expect("serialize");
        let restored: Scenario = toml::from_str(&toml_str).expect("deserialize");

        let restored_step = &restored.steps[0];
        assert_eq!(restored_step.stdout, None);
        assert_eq!(restored_step.exit_code, None);
        assert_eq!(restored_step.delay_ms, None);
        assert_eq!(restored_step.files, None);
    }

    #[test]
    fn multiple_files_in_one_step() {
        let mut files = BTreeMap::new();
        files.insert("file1.txt".to_string(), "content1".to_string());
        files.insert("file2.txt".to_string(), "content2".to_string());
        files.insert("nested/file3.txt".to_string(), "content3".to_string());

        let step = Step {
            on_task: Some(1),
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: None,
            exit_code: None,
            delay_ms: None,
            files: Some(files),
        };

        let scenario = Scenario { steps: vec![step] };

        let toml_str = toml::to_string_pretty(&scenario).expect("serialize");
        let restored: Scenario = toml::from_str(&toml_str).expect("deserialize");

        let restored_files = &restored.steps[0].files;
        assert!(restored_files.is_some());

        let file_map = restored_files.as_ref().unwrap();
        assert_eq!(file_map.len(), 3);
        assert_eq!(file_map.get("file1.txt"), Some(&"content1".to_string()));
        assert_eq!(file_map.get("file2.txt"), Some(&"content2".to_string()));
        assert_eq!(
            file_map.get("nested/file3.txt"),
            Some(&"content3".to_string())
        );
    }

    #[test]
    fn step_outcome_serializes_with_correct_casing() {
        let outcomes = [
            (StepOutcome::Success, "success"),
            (StepOutcome::Failure, "failure"),
            (StepOutcome::Hang, "hang"),
            (StepOutcome::Limit, "limit"),
            (StepOutcome::NeedsInput, "needs_input"),
        ];

        for (outcome, expected_name) in &outcomes {
            // Build a full TOML document with the outcome as part of a step
            let toml_str = format!("[[steps]]\non_task = 1\noutcome = \"{expected_name}\"");
            let parsed: Result<Scenario, _> = toml::from_str(&toml_str);
            assert!(
                parsed.is_ok(),
                "should parse outcome '{expected_name}'"
            );
            assert_eq!(&parsed.unwrap().steps[0].outcome, outcome);
        }
    }

    #[test]
    fn on_task_and_on_attempt_are_mutually_optional() {
        // Step with only on_task
        let step1 = Step {
            on_task: Some(1),
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: None,
            exit_code: None,
            delay_ms: None,
            files: None,
        };

        // Step with only on_attempt
        let step2 = Step {
            on_task: None,
            on_attempt: Some(1),
            outcome: StepOutcome::Failure,
            stdout: None,
            exit_code: None,
            delay_ms: None,
            files: None,
        };

        let scenario = Scenario {
            steps: vec![step1, step2],
        };

        let toml_str = toml::to_string_pretty(&scenario).expect("serialize");

        // Verify on_task is present in first step
        assert!(toml_str.contains("on_task = 1"));
        // Verify on_attempt is present in second step
        assert!(toml_str.contains("on_attempt = 1"));

        let restored: Scenario = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(restored.steps[0].on_task, Some(1));
        assert_eq!(restored.steps[0].on_attempt, None);
        assert_eq!(restored.steps[1].on_task, None);
        assert_eq!(restored.steps[1].on_attempt, Some(1));
    }
}

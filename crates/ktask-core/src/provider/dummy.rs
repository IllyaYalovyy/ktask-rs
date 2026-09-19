//! Dummy provider for deterministic testing and scenario-driven development.
//!
//! The dummy provider replays predefined, deterministic outcomes (success, failure,
//! hang, limit, needs_input) loaded from TOML scenario files. It powers the scenario
//! suite, CI, and offline development without consuming real API tokens.

use crate::provider::{Capabilities, Invocation, Outcome, Provider};
use crate::{Bus, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

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

/// A dummy provider that replays scenarios.
///
/// Consumes steps from a scenario in order, writing files, emitting output,
/// and returning predetermined outcomes. Maintains state to track which step
/// to execute next.
#[derive(Debug)]
pub struct Dummy {
    scenario: Scenario,
    next_step_index: Mutex<usize>,
}

impl Dummy {
    /// Create a new dummy provider with the given scenario.
    #[must_use]
    pub fn new(scenario: Scenario) -> Self {
        Dummy {
            scenario,
            next_step_index: Mutex::new(0),
        }
    }
}

impl Provider for Dummy {
    fn name(&self) -> &'static str {
        "dummy"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_output: false,
            model_selection: false,
            usage_telemetry: false,
        }
    }

    fn invoke(&self, inv: &Invocation, _bus: Option<&Bus>) -> Result<Outcome> {
        let mut next_idx = self.next_step_index.lock().map_err(|_| Error::Provider {
            provider: "dummy".to_string(),
            detail: "mutex poisoned".to_string(),
        })?;

        if *next_idx >= self.scenario.steps.len() {
            return Err(Error::Provider {
                provider: "dummy".to_string(),
                detail: "ran out of steps".to_string(),
            });
        }

        let step = self
            .scenario
            .steps
            .get(*next_idx)
            .ok_or_else(|| Error::Provider {
                provider: "dummy".to_string(),
                detail: "ran out of steps".to_string(),
            })?;
        *next_idx += 1;

        // Handle delay if specified
        if let Some(delay_ms) = step.delay_ms {
            if step.outcome == StepOutcome::Hang {
                // For hang, sleep indefinitely (or until interrupted)
                std::thread::sleep(std::time::Duration::from_secs(u64::MAX));
            } else {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
        } else if step.outcome == StepOutcome::Hang {
            // Hang with no explicit delay
            std::thread::sleep(std::time::Duration::from_secs(u64::MAX));
        }

        // Write files if specified
        if let Some(files) = &step.files {
            for (path, content) in files {
                let file_path = inv.working_dir.join(path);
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(file_path, content)?;
            }
        }

        // Determine exit code based on outcome
        let default_exit_code = match step.outcome {
            StepOutcome::Failure => 1,
            StepOutcome::Success
            | StepOutcome::Hang
            | StepOutcome::Limit
            | StepOutcome::NeedsInput => 0,
        };
        let exit_code = step.exit_code.unwrap_or(default_exit_code);

        let stdout = step.stdout.clone().unwrap_or_default();
        let stderr = String::new();

        Ok(Outcome {
            exit_code,
            stdout,
            stderr,
            usage: None,
            session_id: None,
        })
    }
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
        let result: std::result::Result<Scenario, _> = toml::from_str(toml_str);
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
            let parsed: std::result::Result<Scenario, _> = toml::from_str(&toml_str);
            assert!(parsed.is_ok(), "should parse outcome '{expected_name}'");
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

    #[test]
    fn dummy_provider_executes_first_step() {
        use crate::provider::Provider;

        let scenario = Scenario {
            steps: vec![Step {
                on_task: Some(1),
                on_attempt: None,
                outcome: StepOutcome::Success,
                stdout: Some("Success output".to_string()),
                exit_code: None,
                delay_ms: None,
                files: None,
            }],
        };

        let dummy = Dummy::new(scenario);
        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: std::path::PathBuf::from("/tmp"),
        };

        let outcome = dummy.invoke(&inv, None).expect("invoke succeeded");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "Success output");
    }

    #[test]
    fn dummy_provider_runs_out_of_steps() {
        use crate::provider::Provider;

        let scenario = Scenario {
            steps: vec![Step {
                on_task: Some(1),
                on_attempt: None,
                outcome: StepOutcome::Success,
                stdout: None,
                exit_code: None,
                delay_ms: None,
                files: None,
            }],
        };

        let dummy = Dummy::new(scenario);
        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: std::path::PathBuf::from("/tmp"),
        };

        // First call should succeed
        let _ = dummy.invoke(&inv, None).expect("first invoke succeeded");

        // Second call should fail
        let err = dummy.invoke(&inv, None).expect_err("second invoke failed");
        assert!(err.to_string().contains("ran out of steps"));
    }

    #[test]
    fn dummy_provider_writes_files() {
        use crate::provider::Provider;
        use tempfile::TempDir;

        let mut files = BTreeMap::new();
        files.insert("test.txt".to_string(), "Hello".to_string());

        let scenario = Scenario {
            steps: vec![Step {
                on_task: Some(1),
                on_attempt: None,
                outcome: StepOutcome::Success,
                stdout: None,
                exit_code: None,
                delay_ms: None,
                files: Some(files),
            }],
        };

        let dummy = Dummy::new(scenario);
        let temp_dir = TempDir::new().expect("temp dir created");
        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: temp_dir.path().to_path_buf(),
        };

        let _ = dummy.invoke(&inv, None).expect("invoke succeeded");

        let file_path = temp_dir.path().join("test.txt");
        assert!(file_path.exists(), "file was written");
        let content = std::fs::read_to_string(&file_path).expect("read file");
        assert_eq!(content, "Hello");
    }

    #[test]
    fn dummy_provider_failure_exit_code() {
        use crate::provider::Provider;

        let scenario = Scenario {
            steps: vec![Step {
                on_task: Some(1),
                on_attempt: None,
                outcome: StepOutcome::Failure,
                stdout: None,
                exit_code: None,
                delay_ms: None,
                files: None,
            }],
        };

        let dummy = Dummy::new(scenario);
        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: std::path::PathBuf::from("/tmp"),
        };

        let outcome = dummy.invoke(&inv, None).expect("invoke succeeded");
        assert_eq!(outcome.exit_code, 1);
    }

    #[test]
    fn dummy_provider_custom_exit_code() {
        use crate::provider::Provider;

        let scenario = Scenario {
            steps: vec![Step {
                on_task: Some(1),
                on_attempt: None,
                outcome: StepOutcome::Failure,
                stdout: None,
                exit_code: Some(42),
                delay_ms: None,
                files: None,
            }],
        };

        let dummy = Dummy::new(scenario);
        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: std::path::PathBuf::from("/tmp"),
        };

        let outcome = dummy.invoke(&inv, None).expect("invoke succeeded");
        assert_eq!(outcome.exit_code, 42);
    }

    #[test]
    fn dummy_provider_consumes_steps_in_order() {
        use crate::provider::Provider;

        let scenario = Scenario {
            steps: vec![
                Step {
                    on_task: Some(1),
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("First".to_string()),
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
                Step {
                    on_task: Some(2),
                    on_attempt: None,
                    outcome: StepOutcome::Failure,
                    stdout: Some("Second".to_string()),
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
                Step {
                    on_task: Some(3),
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("Third".to_string()),
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
            ],
        };

        let dummy = Dummy::new(scenario);
        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: std::path::PathBuf::from("/tmp"),
        };

        let o1 = dummy.invoke(&inv, None).expect("first invoke succeeded");
        assert_eq!(o1.stdout, "First");
        assert_eq!(o1.exit_code, 0);

        let o2 = dummy.invoke(&inv, None).expect("second invoke succeeded");
        assert_eq!(o2.stdout, "Second");
        assert_eq!(o2.exit_code, 1);

        let o3 = dummy.invoke(&inv, None).expect("third invoke succeeded");
        assert_eq!(o3.stdout, "Third");
        assert_eq!(o3.exit_code, 0);
    }

    #[test]
    fn dummy_provider_deterministic_across_invocations() {
        use crate::provider::Provider;

        let scenario = Scenario {
            steps: vec![
                Step {
                    on_task: Some(1),
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("Output A".to_string()),
                    exit_code: Some(42),
                    delay_ms: None,
                    files: None,
                },
                Step {
                    on_task: Some(2),
                    on_attempt: None,
                    outcome: StepOutcome::Failure,
                    stdout: Some("Output B".to_string()),
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
            ],
        };

        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: std::path::PathBuf::from("/tmp"),
        };

        // Create two Dummy providers with the same scenario
        let dummy1 = Dummy::new(scenario.clone());
        let dummy2 = Dummy::new(scenario.clone());

        // Run both in sequence and verify they produce identical outputs
        let o1a = dummy1
            .invoke(&inv, None)
            .expect("dummy1 first invoke succeeded");
        let o2a = dummy2
            .invoke(&inv, None)
            .expect("dummy2 first invoke succeeded");

        assert_eq!(o1a.exit_code, o2a.exit_code);
        assert_eq!(o1a.stdout, o2a.stdout);
        assert_eq!(o1a.stderr, o2a.stderr);

        let o1b = dummy1
            .invoke(&inv, None)
            .expect("dummy1 second invoke succeeded");
        let o2b = dummy2
            .invoke(&inv, None)
            .expect("dummy2 second invoke succeeded");

        assert_eq!(o1b.exit_code, o2b.exit_code);
        assert_eq!(o1b.stdout, o2b.stdout);
        assert_eq!(o1b.stderr, o2b.stderr);
    }

    #[test]
    fn dummy_provider_all_step_outcomes() {
        use crate::provider::Provider;

        let scenario = Scenario {
            steps: vec![
                Step {
                    on_task: Some(1),
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: None,
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
                Step {
                    on_task: Some(2),
                    on_attempt: None,
                    outcome: StepOutcome::Failure,
                    stdout: None,
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
                Step {
                    on_task: Some(3),
                    on_attempt: None,
                    outcome: StepOutcome::Limit,
                    stdout: None,
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
                Step {
                    on_task: Some(4),
                    on_attempt: None,
                    outcome: StepOutcome::NeedsInput,
                    stdout: None,
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
            ],
        };

        let dummy = Dummy::new(scenario);
        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: std::path::PathBuf::from("/tmp"),
        };

        // Success outcome
        let o1 = dummy.invoke(&inv, None).expect("invoke 1 succeeded");
        assert_eq!(o1.exit_code, 0);

        // Failure outcome
        let o2 = dummy.invoke(&inv, None).expect("invoke 2 succeeded");
        assert_eq!(o2.exit_code, 1);

        // Limit outcome (defaults to 0)
        let o3 = dummy.invoke(&inv, None).expect("invoke 3 succeeded");
        assert_eq!(o3.exit_code, 0);

        // NeedsInput outcome (defaults to 0)
        let o4 = dummy.invoke(&inv, None).expect("invoke 4 succeeded");
        assert_eq!(o4.exit_code, 0);
    }

    #[test]
    fn dummy_provider_name_and_capabilities() {
        use crate::provider::Provider;

        let scenario = Scenario { steps: vec![] };
        let dummy = Dummy::new(scenario);

        assert_eq!(dummy.name(), "dummy");
        let caps = dummy.capabilities();
        assert!(!caps.structured_output);
        assert!(!caps.model_selection);
        assert!(!caps.usage_telemetry);
    }
}

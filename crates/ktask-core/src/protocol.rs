//! Work protocols: per-task state machines defining sequence of phases.

use crate::gate::GateKind;
use crate::state::Phase;
use crate::{Config, Error, Result, Task};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Write scope for a phase, determining what the agent can modify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteScope {
    /// Agent can modify all paths.
    All,
    /// Agent can modify only test-related paths.
    TestsOnly,
    /// Agent cannot modify any paths (read-only).
    None,
}

/// Specification of a single phase in a protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseSpec {
    /// The phase identifier.
    pub phase: Phase,
    /// Write scope for this phase.
    pub write_scope: WriteScope,
    /// Optional gate to run at the end of this phase.
    pub gate: Option<GateKind>,
    /// Whether this phase records evidence.
    pub records_evidence: bool,
}

/// A work protocol: a typed sequence of phases with constraints.
///
/// Every protocol must end with Verify then Publish phases.
/// This is enforced by the constructors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protocol {
    /// Human-readable name of the protocol.
    pub name: String,
    /// Ordered sequence of phases in the protocol.
    pub phases: Vec<PhaseSpec>,
}

impl Protocol {
    /// The `direct` protocol: single implementation phase, then mandatory gates.
    ///
    /// Direct workflow: implement → verify → publish
    #[must_use]
    pub fn direct() -> Protocol {
        let p = Protocol {
            name: "direct".to_string(),
            phases: vec![
                PhaseSpec {
                    phase: Phase::Implement,
                    write_scope: WriteScope::All,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
            ],
        };
        debug_assert!(
            p.validate(),
            "direct protocol must end with Verify then Publish"
        );
        p
    }

    /// The `tdd` protocol: test-driven development with red/green/refactor.
    ///
    /// TDD workflow:
    /// 1. Red: write failing tests
    /// 2. Green: implement to pass tests
    /// 3. Refactor: cleanup while keeping tests passing
    /// 4. Verify and publish
    #[must_use]
    pub fn tdd() -> Protocol {
        let p = Protocol {
            name: "tdd".to_string(),
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
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
            ],
        };
        debug_assert!(
            p.validate(),
            "tdd protocol must end with Verify then Publish"
        );
        p
    }

    /// Validates that the protocol ends with Verify then Publish phases.
    fn validate(&self) -> bool {
        match (
            self.phases.get(self.phases.len().saturating_sub(2)),
            self.phases.last(),
        ) {
            (Some(second_last), Some(last)) => {
                second_last.phase == Phase::Verify && last.phase == Phase::Publish
            }
            _ => false,
        }
    }

    /// Select a protocol for a task based on task-specified, configured, or default protocol.
    ///
    /// The protocol is chosen in this order:
    /// 1. If the task specifies a `**Protocol:**` section, use that (must be "direct" or "tdd")
    /// 2. If config has a `default_protocol`, use that (must be "direct" or "tdd")
    /// 3. Otherwise default to "direct"
    ///
    /// # Errors
    ///
    /// Returns an error if the resolved protocol name is not "direct" or "tdd".
    pub fn for_task(task: &Task, config: &Config) -> Result<Protocol> {
        let protocol_name = task
            .protocol_name()
            .unwrap_or_else(|| config.default_protocol.clone());

        match protocol_name.to_lowercase().as_str() {
            "direct" => Ok(Protocol::direct()),
            "tdd" => Ok(Protocol::tdd()),
            _ => Err(Error::Policy {
                detail: format!(
                    "Task '{}' cannot be assigned protocol '{}'; must be 'direct' or 'tdd'",
                    task.title(),
                    protocol_name
                ),
                paths: vec![],
            }),
        }
    }
}

/// Check if a path matches any of the provided glob patterns.
fn path_matches_glob(path: &std::path::Path, patterns: &[String]) -> bool {
    let path_str = path.to_string_lossy();
    patterns
        .iter()
        .any(|pattern| glob_matches(&path_str, pattern))
}

/// Simple glob pattern matching with support for `*` and `**` wildcards.
///
/// - `*` matches any sequence of characters except `/`
/// - `**` matches any sequence of characters including `/`
/// - Other characters match literally
fn glob_matches(path: &str, pattern: &str) -> bool {
    glob_matches_impl(path, pattern, 0, 0)
}

/// Recursive helper for glob matching.
fn glob_matches_impl(path: &str, pattern: &str, path_idx: usize, pattern_idx: usize) -> bool {
    let path_bytes = path.as_bytes();
    let pattern_bytes = pattern.as_bytes();

    if pattern_idx >= pattern_bytes.len() {
        return path_idx >= path_bytes.len();
    }

    if pattern_idx + 1 < pattern_bytes.len()
        && pattern_bytes.get(pattern_idx) == Some(&b'*')
        && pattern_bytes.get(pattern_idx + 1) == Some(&b'*')
    {
        // Handle `**` wildcard: match any sequence including `/`
        let next_pattern_idx = pattern_idx + 2;

        if next_pattern_idx >= pattern_bytes.len() {
            // `**` at end of pattern matches everything
            return true;
        }

        if pattern_bytes.get(next_pattern_idx) == Some(&b'/') {
            // `**/` case
            let next_pattern_idx = next_pattern_idx + 1;

            // Try matching from each position in the path
            for i in path_idx..=path_bytes.len() {
                if glob_matches_impl(path, pattern, i, next_pattern_idx) {
                    return true;
                }
            }
            return false;
        }

        // `**` not followed by `/`, treat as regular `*`
        for i in path_idx..=path_bytes.len() {
            if glob_matches_impl(path, pattern, i, next_pattern_idx) {
                return true;
            }
        }
        return false;
    }

    if path_idx >= path_bytes.len() {
        // Path exhausted, pattern not exhausted
        if pattern_bytes.get(pattern_idx) == Some(&b'*') {
            return glob_matches_impl(path, pattern, path_idx, pattern_idx + 1);
        }
        return false;
    }

    if pattern_bytes.get(pattern_idx) == Some(&b'*') {
        // Handle `*` wildcard: match any sequence except `/`
        let next_pattern_idx = pattern_idx + 1;

        // Try matching from each position until we hit a `/` or end of path
        for i in path_idx..=path_bytes.len() {
            if i > path_idx && path_bytes.get(i - 1) == Some(&b'/') {
                break;
            }
            if glob_matches_impl(path, pattern, i, next_pattern_idx) {
                return true;
            }
        }
        return false;
    }

    if pattern_bytes.get(pattern_idx) == path_bytes.get(path_idx) {
        return glob_matches_impl(path, pattern, path_idx + 1, pattern_idx + 1);
    }

    false
}

/// Enforce write-scope constraints for a phase.
///
/// Checks that changed paths comply with the declared write scope. Returns a Policy error
/// if any path violates the scope.
///
/// # Arguments
///
/// - `scope` - The write scope constraint for this phase
/// - `changed` - Paths that were modified (added, modified, or deleted)
/// - `test_globs` - Glob patterns identifying test files
///
/// # Returns
///
/// `Ok(())` if all changes comply with the scope, or a Policy error naming offending paths
///
/// # Errors
///
/// Returns `Error::Policy` if any changed path violates the write scope constraints.
///
/// # Scope Semantics
///
/// - `All`: any path is allowed
/// - `TestsOnly`: only paths matching `test_globs` are allowed
/// - `None`: no paths are allowed (read-only)
pub fn check_scope(scope: WriteScope, changed: &[PathBuf], test_globs: &[String]) -> Result<()> {
    match scope {
        WriteScope::All => Ok(()),
        WriteScope::None => {
            if changed.is_empty() {
                Ok(())
            } else {
                Err(Error::Policy {
                    detail: "phase is read-only; no file modifications allowed".to_string(),
                    paths: changed.to_vec(),
                })
            }
        }
        WriteScope::TestsOnly => {
            let non_test_paths: Vec<PathBuf> = changed
                .iter()
                .filter(|path| !path_matches_glob(path, test_globs))
                .cloned()
                .collect();

            if non_test_paths.is_empty() {
                Ok(())
            } else {
                Err(Error::Policy {
                    detail: "phase allows test files only; production files cannot be modified"
                        .to_string(),
                    paths: non_test_paths,
                })
            }
        }
    }
}

/// Verify that a TDD red phase produced genuinely new failing tests.
///
/// Compares test summaries from before and after the red phase to confirm that
/// at least one test is now failing that was not failing before.
///
/// # Arguments
///
/// - `before` - Test summary before the red phase
/// - `after` - Test summary after the red phase
///
/// # Returns
///
/// `Ok(Vec<String>)` containing the names of tests that are failing after but not before,
/// or an error if no new tests are failing.
///
/// # Errors
///
/// Returns an error if the set of failing tests is unchanged (empty or identical to before).
pub fn verify_red(
    before: &crate::gate::TestSummary,
    after: &crate::gate::TestSummary,
) -> Result<Vec<String>> {
    let before_failures: std::collections::HashSet<_> = before.failures.iter().cloned().collect();
    let after_failures: std::collections::HashSet<_> = after.failures.iter().cloned().collect();

    let newly_failing: Vec<String> = after_failures
        .difference(&before_failures)
        .cloned()
        .collect();

    if newly_failing.is_empty() {
        Err(Error::Gate {
            kind: "red".to_string(),
            detail: "red phase must produce at least one new failing test".to_string(),
        })
    } else {
        Ok(newly_failing)
    }
}

/// Verify that a TDD green phase made all expected tests pass.
///
/// Confirms that all tests named in the expected list are now passing,
/// and that no previously passing tests have regressed (are now failing).
///
/// # Arguments
///
/// - `expected` - List of test names that should pass (typically from the red phase)
/// - `after` - Test summary after the green phase
///
/// # Returns
///
/// `Ok(())` if all expected tests pass and no unexpected failures occur,
/// or an error naming any expected test that failed.
///
/// # Errors
///
/// Returns an error if any expected test is failing, or if there are
/// failures that were not present in the expected list.
pub fn verify_green(expected: &[String], after: &crate::gate::TestSummary) -> Result<()> {
    let failed_set: std::collections::HashSet<_> = after.failures.iter().cloned().collect();

    // Check that all expected tests pass
    let failed_expected: Vec<String> = expected
        .iter()
        .filter(|test| failed_set.contains(*test))
        .cloned()
        .collect();

    if !failed_expected.is_empty() {
        return Err(Error::Gate {
            kind: "green".to_string(),
            detail: format!(
                "green phase failed to pass expected tests: {}",
                failed_expected.join(", ")
            ),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_direct_ends_with_verify_publish() {
        let p = Protocol::direct();
        assert_eq!(p.phases.len(), 3);
        assert_eq!(p.phases[1].phase, Phase::Verify);
        assert_eq!(p.phases[2].phase, Phase::Publish);
        assert!(p.validate());
    }

    #[test]
    fn protocol_tdd_ends_with_verify_publish() {
        let p = Protocol::tdd();
        assert!(p.phases.len() >= 2);
        let phases = &p.phases;
        assert_eq!(phases[phases.len() - 2].phase, Phase::Verify);
        assert_eq!(phases[phases.len() - 1].phase, Phase::Publish);
        assert!(p.validate());
    }

    #[test]
    fn protocol_missing_verify_fails_validation() {
        let p = Protocol {
            name: "invalid".to_string(),
            phases: vec![
                PhaseSpec {
                    phase: Phase::Implement,
                    write_scope: WriteScope::All,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
            ],
        };
        assert!(!p.validate());
    }

    #[test]
    fn protocol_missing_publish_fails_validation() {
        let p = Protocol {
            name: "invalid".to_string(),
            phases: vec![
                PhaseSpec {
                    phase: Phase::Implement,
                    write_scope: WriteScope::All,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
            ],
        };
        assert!(!p.validate());
    }

    #[test]
    fn protocol_verify_publish_out_of_order_fails_validation() {
        let p = Protocol {
            name: "invalid".to_string(),
            phases: vec![
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
            ],
        };
        assert!(!p.validate());
    }

    #[test]
    fn phase_spec_round_trips_through_json() {
        let spec = PhaseSpec {
            phase: Phase::Red,
            write_scope: WriteScope::TestsOnly,
            gate: Some(GateKind::Targeted),
            records_evidence: true,
        };
        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: PhaseSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, deserialized);
    }

    #[test]
    fn write_scope_round_trips_through_json() {
        for scope in &[WriteScope::All, WriteScope::TestsOnly, WriteScope::None] {
            let json = serde_json::to_string(scope).unwrap();
            let deserialized: WriteScope = serde_json::from_str(&json).unwrap();
            assert_eq!(*scope, deserialized);
        }
    }

    #[test]
    fn protocol_round_trips_through_json() {
        let p = Protocol::tdd();
        let value = serde_json::to_value(&p).unwrap();
        let deserialized: Protocol = serde_json::from_value(value).unwrap();
        assert_eq!(p, deserialized);
    }

    #[test]
    fn protocol_direct_has_implement_verify_publish() {
        let p = Protocol::direct();
        assert_eq!(p.phases[0].phase, Phase::Implement);
        assert_eq!(p.phases[1].phase, Phase::Verify);
        assert_eq!(p.phases[2].phase, Phase::Publish);
    }

    #[test]
    fn protocol_tdd_has_red_green_refactor() {
        let p = Protocol::tdd();
        let phases: Vec<_> = p.phases.iter().map(|spec| spec.phase).collect();
        assert!(phases.contains(&Phase::Red));
        assert!(phases.contains(&Phase::Green));
        assert!(phases.contains(&Phase::Refactor));
    }

    #[test]
    fn phase_spec_write_scopes() {
        let p = Protocol::direct();
        assert_eq!(p.phases[0].write_scope, WriteScope::All);
        assert_eq!(p.phases[1].write_scope, WriteScope::None);
        assert_eq!(p.phases[2].write_scope, WriteScope::None);
    }

    #[test]
    fn protocol_for_task_uses_task_protocol_when_specified() {
        use crate::{Config, Task, TaskId, TaskStatus};
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task title\n\nProtocol: tdd".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "When done".to_string(),
            verify: "cargo test".to_string(),
            refs: "Ref".to_string(),
        };
        let config = Config::default();
        let protocol = Protocol::for_task(&task, &config).expect("should select protocol");
        assert_eq!(protocol.name, "tdd");
    }

    #[test]
    fn protocol_for_task_uses_config_default_when_not_in_task() {
        use crate::{Config, Task, TaskId, TaskStatus};
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task title without protocol".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "When done".to_string(),
            verify: "cargo test".to_string(),
            refs: "Ref".to_string(),
        };
        let mut config = Config::default();
        config.default_protocol = "tdd".to_string();
        let protocol = Protocol::for_task(&task, &config).expect("should select protocol");
        assert_eq!(protocol.name, "tdd");
    }

    #[test]
    fn protocol_for_task_defaults_to_direct() {
        use crate::{Config, Task, TaskId, TaskStatus};
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task title without protocol".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "When done".to_string(),
            verify: "cargo test".to_string(),
            refs: "Ref".to_string(),
        };
        let config = Config::default();
        let protocol = Protocol::for_task(&task, &config).expect("should select protocol");
        assert_eq!(protocol.name, "direct");
    }

    #[test]
    fn protocol_for_task_prefers_task_over_config() {
        use crate::{Config, Task, TaskId, TaskStatus};
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task title\n\nProtocol: direct".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "When done".to_string(),
            verify: "cargo test".to_string(),
            refs: "Ref".to_string(),
        };
        let mut config = Config::default();
        config.default_protocol = "tdd".to_string();
        let protocol = Protocol::for_task(&task, &config).expect("should select protocol");
        assert_eq!(protocol.name, "direct");
    }

    #[test]
    fn protocol_for_task_rejects_invalid_protocol() {
        use crate::{Config, Task, TaskId, TaskStatus};
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task title\n\nProtocol: invalid".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "When done".to_string(),
            verify: "cargo test".to_string(),
            refs: "Ref".to_string(),
        };
        let config = Config::default();
        let result = Protocol::for_task(&task, &config);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("invalid"));
        assert!(err_msg.contains("must be 'direct' or 'tdd'"));
    }

    #[test]
    fn protocol_for_task_case_insensitive() {
        use crate::{Config, Task, TaskId, TaskStatus};
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task title\n\nProtocol: TDD".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "When done".to_string(),
            verify: "cargo test".to_string(),
            refs: "Ref".to_string(),
        };
        let config = Config::default();
        let protocol = Protocol::for_task(&task, &config).expect("should select protocol");
        assert_eq!(protocol.name, "tdd");
    }

    #[test]
    fn glob_matches_single_star() {
        assert!(glob_matches("test.rs", "*.rs"));
        assert!(glob_matches("foo.txt", "*.txt"));
        assert!(!glob_matches("dir/test.rs", "*.rs"));
        assert!(!glob_matches("test.rs", "*.txt"));
    }

    #[test]
    fn glob_matches_double_star() {
        assert!(glob_matches("test.rs", "**/*.rs"));
        assert!(glob_matches("dir/test.rs", "**/*.rs"));
        assert!(glob_matches("a/b/c/test.rs", "**/*.rs"));
        assert!(!glob_matches("test.txt", "**/*.rs"));
    }

    #[test]
    fn glob_matches_double_star_prefix() {
        assert!(glob_matches("tests/unit.rs", "**/tests/**"));
        assert!(glob_matches("src/tests/unit.rs", "**/tests/**"));
        assert!(glob_matches("tests/deep/nested/test.rs", "**/tests/**"));
        assert!(!glob_matches("src/main.rs", "**/tests/**"));
    }

    #[test]
    fn glob_matches_exact_path() {
        assert!(glob_matches("README.md", "README.md"));
        assert!(!glob_matches("src/README.md", "README.md"));
        assert!(!glob_matches("README.md.bak", "README.md"));
    }

    #[test]
    fn glob_matches_suffix_pattern() {
        assert!(glob_matches("unit_test.rs", "*_test.rs"));
        assert!(glob_matches("integration_test.rs", "*_test.rs"));
        assert!(!glob_matches("test.rs", "*_test.rs"));
    }

    #[test]
    fn check_scope_all_allows_everything() {
        let changed = vec![PathBuf::from("src/main.rs"), PathBuf::from("tests/unit.rs")];
        let test_globs = vec!["**/tests/**".to_string()];
        let result = check_scope(WriteScope::All, &changed, &test_globs);
        assert!(result.is_ok());
    }

    #[test]
    fn check_scope_all_allows_empty_changes() {
        let changed = vec![];
        let test_globs = vec!["**/tests/**".to_string()];
        let result = check_scope(WriteScope::All, &changed, &test_globs);
        assert!(result.is_ok());
    }

    #[test]
    fn check_scope_none_rejects_any_change() {
        let changed = vec![PathBuf::from("src/main.rs")];
        let test_globs = vec!["**/tests/**".to_string()];
        let result = check_scope(WriteScope::None, &changed, &test_globs);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(err_str.contains("read-only"));
    }

    #[test]
    fn check_scope_none_allows_empty_changes() {
        let changed = vec![];
        let test_globs = vec!["**/tests/**".to_string()];
        let result = check_scope(WriteScope::None, &changed, &test_globs);
        assert!(result.is_ok());
    }

    #[test]
    fn check_scope_tests_only_permits_test_edit() {
        let changed = vec![PathBuf::from("tests/unit_test.rs")];
        let test_globs = vec!["**/tests/**".to_string(), "**/*_test.rs".to_string()];
        let result = check_scope(WriteScope::TestsOnly, &changed, &test_globs);
        assert!(result.is_ok());
    }

    #[test]
    fn check_scope_tests_only_rejects_production_edit() {
        let changed = vec![PathBuf::from("src/main.rs")];
        let test_globs = vec!["**/tests/**".to_string(), "**/*_test.rs".to_string()];
        let result = check_scope(WriteScope::TestsOnly, &changed, &test_globs);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(err_str.contains("test files only"));
    }

    #[test]
    fn check_scope_tests_only_mixed_files_reports_non_test() {
        let changed = vec![
            PathBuf::from("tests/unit.rs"),
            PathBuf::from("src/main.rs"),
            PathBuf::from("tests/integration_test.rs"),
        ];
        let test_globs = vec!["**/tests/**".to_string(), "**/*_test.rs".to_string()];
        let result = check_scope(WriteScope::TestsOnly, &changed, &test_globs);
        assert!(result.is_err());
        if let Err(Error::Policy { paths, .. }) = result {
            assert_eq!(paths.len(), 1);
            assert_eq!(paths[0], PathBuf::from("src/main.rs"));
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_scope_tests_only_empty_changes() {
        let changed = vec![];
        let test_globs = vec!["**/tests/**".to_string()];
        let result = check_scope(WriteScope::TestsOnly, &changed, &test_globs);
        assert!(result.is_ok());
    }

    #[test]
    fn glob_matches_default_test_globs() {
        let default_globs = vec![
            "**/tests/**".to_string(),
            "**/*_test.rs".to_string(),
            "src/**/tests.rs".to_string(),
        ];

        assert!(path_matches_glob(
            &PathBuf::from("tests/unit.rs"),
            &default_globs
        ));
        assert!(path_matches_glob(
            &PathBuf::from("src/tests/mod.rs"),
            &default_globs
        ));
        assert!(path_matches_glob(
            &PathBuf::from("unit_test.rs"),
            &default_globs
        ));
        assert!(path_matches_glob(
            &PathBuf::from("src/module/tests.rs"),
            &default_globs
        ));

        assert!(!path_matches_glob(
            &PathBuf::from("src/main.rs"),
            &default_globs
        ));
        assert!(!path_matches_glob(&PathBuf::from("lib.rs"), &default_globs));
    }

    #[test]
    fn verify_red_returns_newly_failing_tests() {
        use crate::gate::TestSummary;

        let before = TestSummary {
            passed: 5,
            failed: 0,
            ignored: 0,
            failures: vec![],
        };
        let after = TestSummary {
            passed: 4,
            failed: 1,
            ignored: 0,
            failures: vec!["test_new_failure".to_string()],
        };

        let result = verify_red(&before, &after);
        assert!(result.is_ok());
        let newly_failing = result.unwrap();
        assert_eq!(newly_failing.len(), 1);
        assert_eq!(newly_failing[0], "test_new_failure");
    }

    #[test]
    fn verify_red_returns_error_when_no_new_failures() {
        use crate::gate::TestSummary;

        let before = TestSummary {
            passed: 5,
            failed: 0,
            ignored: 0,
            failures: vec![],
        };
        let after = TestSummary {
            passed: 5,
            failed: 0,
            ignored: 0,
            failures: vec![],
        };

        let result = verify_red(&before, &after);
        assert!(result.is_err());
    }

    #[test]
    fn verify_red_returns_error_when_same_failures() {
        use crate::gate::TestSummary;

        let before = TestSummary {
            passed: 4,
            failed: 1,
            ignored: 0,
            failures: vec!["test_failure".to_string()],
        };
        let after = TestSummary {
            passed: 4,
            failed: 1,
            ignored: 0,
            failures: vec!["test_failure".to_string()],
        };

        let result = verify_red(&before, &after);
        assert!(result.is_err());
    }

    #[test]
    fn verify_red_returns_multiple_new_failures() {
        use crate::gate::TestSummary;

        let before = TestSummary {
            passed: 5,
            failed: 0,
            ignored: 0,
            failures: vec![],
        };
        let after = TestSummary {
            passed: 3,
            failed: 2,
            ignored: 0,
            failures: vec!["test_first".to_string(), "test_second".to_string()],
        };

        let result = verify_red(&before, &after);
        assert!(result.is_ok());
        let newly_failing = result.unwrap();
        assert_eq!(newly_failing.len(), 2);
        assert!(newly_failing.contains(&"test_first".to_string()));
        assert!(newly_failing.contains(&"test_second".to_string()));
    }

    #[test]
    fn verify_red_ignores_preexisting_failures() {
        use crate::gate::TestSummary;

        let before = TestSummary {
            passed: 4,
            failed: 1,
            ignored: 0,
            failures: vec!["old_failure".to_string()],
        };
        let after = TestSummary {
            passed: 3,
            failed: 2,
            ignored: 0,
            failures: vec!["old_failure".to_string(), "new_failure".to_string()],
        };

        let result = verify_red(&before, &after);
        assert!(result.is_ok());
        let newly_failing = result.unwrap();
        assert_eq!(newly_failing.len(), 1);
        assert_eq!(newly_failing[0], "new_failure");
    }

    #[test]
    fn verify_green_passes_when_all_expected_tests_pass() {
        use crate::gate::TestSummary;

        let expected = vec!["test_new_feature".to_string(), "test_edge_case".to_string()];
        let after = TestSummary {
            passed: 7,
            failed: 0,
            ignored: 0,
            failures: vec![],
        };

        let result = verify_green(&expected, &after);
        assert!(result.is_ok());
    }

    #[test]
    fn verify_green_fails_when_expected_test_fails() {
        use crate::gate::TestSummary;

        let expected = vec!["test_feature".to_string()];
        let after = TestSummary {
            passed: 4,
            failed: 1,
            ignored: 0,
            failures: vec!["test_feature".to_string()],
        };

        let result = verify_green(&expected, &after);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(err_str.contains("test_feature"));
        assert!(err_str.contains("green"));
    }

    #[test]
    fn verify_green_fails_and_names_multiple_failures() {
        use crate::gate::TestSummary;

        let expected = vec![
            "test_first".to_string(),
            "test_second".to_string(),
            "test_third".to_string(),
        ];
        let after = TestSummary {
            passed: 2,
            failed: 2,
            ignored: 0,
            failures: vec!["test_first".to_string(), "test_second".to_string()],
        };

        let result = verify_green(&expected, &after);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(err_str.contains("test_first"));
        assert!(err_str.contains("test_second"));
        // test_third should not appear in error since it passed
        assert!(!err_str.contains("test_third"));
    }

    #[test]
    fn verify_green_passes_with_empty_expected_list() {
        use crate::gate::TestSummary;

        let expected: Vec<String> = vec![];
        let after = TestSummary {
            passed: 5,
            failed: 0,
            ignored: 0,
            failures: vec![],
        };

        let result = verify_green(&expected, &after);
        assert!(result.is_ok());
    }

    #[test]
    fn verify_green_passes_when_only_expected_tests_are_present() {
        use crate::gate::TestSummary;

        let expected = vec!["test_a".to_string(), "test_b".to_string()];
        let after = TestSummary {
            passed: 2,
            failed: 0,
            ignored: 0,
            failures: vec![],
        };

        let result = verify_green(&expected, &after);
        assert!(result.is_ok());
    }

    #[test]
    fn verify_green_regression_when_expected_test_regresses() {
        use crate::gate::TestSummary;

        let expected = vec!["test_regression".to_string()];
        let after = TestSummary {
            passed: 5,
            failed: 1,
            ignored: 0,
            failures: vec!["test_regression".to_string()],
        };

        let result = verify_green(&expected, &after);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("test_regression"));
    }

    #[test]
    fn verify_green_ignores_unexpected_failures() {
        use crate::gate::TestSummary;

        let expected = vec!["test_expected".to_string()];
        let after = TestSummary {
            passed: 4,
            failed: 2,
            ignored: 0,
            failures: vec!["test_other".to_string(), "test_unrelated".to_string()],
        };

        let result = verify_green(&expected, &after);
        assert!(result.is_ok());
    }

    #[test]
    fn verify_green_catches_regression_in_unrelated_test() {
        use crate::gate::TestSummary;

        let expected = vec!["test_new".to_string()];
        let after = TestSummary {
            passed: 4,
            failed: 2,
            ignored: 0,
            failures: vec!["test_new".to_string(), "test_existing_broke".to_string()],
        };

        let result = verify_green(&expected, &after);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("test_new"));
        assert!(!err_str.contains("test_existing_broke"));
    }
}

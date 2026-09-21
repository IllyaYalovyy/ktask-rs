//! Rerun-gate command: re-run a completion gate.

use crate::render;
use ktask_core::{
    Journal, RunOutcome, gate::{GateKind, profile_from, run_completion_set, run_gate},
    ids::TaskId, queue,
};

pub(crate) fn run(
    project: Option<ktask_core::Project>,
    config: Option<ktask_core::Config>,
    task: String,
    gate: Option<String>,
    json: bool,
) -> RunOutcome {
    let Some(proj) = project else {
        return RunOutcome::Usage {
            detail: "no project found".to_string(),
        };
    };

    let Some(cfg) = config else {
        return RunOutcome::Usage {
            detail: "no configuration found".to_string(),
        };
    };

    let tasks = match queue::load(&proj) {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error loading queue: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let task_id = match task.parse::<u32>() {
        Ok(id) => TaskId::new(id),
        Err(_) => {
            return RunOutcome::Usage {
                detail: format!("invalid task id: {task}"),
            };
        }
    };

    // Verify task exists
    if !tasks.iter().any(|t| t.id == task_id) {
        return RunOutcome::Usage {
            detail: format!("task {task_id} not found in queue"),
        };
    }

    let journal = match Journal::open_for(&proj) {
        Ok(j) => j,
        Err(e) => {
            render::progress(format_args!("error opening journal: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let states = match journal.all_states() {
        Ok(s) => s,
        Err(e) => {
            render::progress(format_args!("error loading states: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    // Get the task's current state to find the latest attempt
    let task_state = states.get(&task_id);
    if task_state.is_none() {
        return RunOutcome::Usage {
            detail: format!("task {task_id} has no state"),
        };
    }

    // Build verification profile from config
    let profile = match profile_from(&cfg) {
        Ok(p) => p,
        Err(e) => {
            render::progress(format_args!("error building verification profile: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    // Get the worktree path for this task
    let worktree_path = proj.root.join(format!("task-{task_id}"));

    // Check if worktree exists
    if !worktree_path.exists() {
        return RunOutcome::Usage {
            detail: format!("task worktree not found at {}", worktree_path.display()),
        };
    }

    // Run gates
    let results = if let Some(gate_kind_str) = gate {
        // Parse gate kind from string
        let kind = parse_gate_kind(&gate_kind_str);
        match kind {
            Some(k) => {
                match profile.get(k) {
                    Some(gate_def) => {
                        match run_gate(gate_def, &worktree_path, None) {
                            Ok(result) => vec![result],
                            Err(e) => {
                                render::progress(format_args!("error running gate: {e}"));
                                return RunOutcome::Usage {
                                    detail: format!("{e}"),
                                };
                            }
                        }
                    }
                    None => {
                        return RunOutcome::Usage {
                            detail: format!("gate {gate_kind_str} not configured"),
                        };
                    }
                }
            }
            None => {
                return RunOutcome::Usage {
                    detail: format!("invalid gate kind: {gate_kind_str}"),
                };
            }
        }
    } else {
        // Run the whole completion set
        match run_completion_set(&profile, &worktree_path, "HEAD", None) {
            Ok(results) => results,
            Err(e) => {
                render::progress(format_args!("error running gates: {e}"));
                return RunOutcome::Usage {
                    detail: format!("{e}"),
                };
            }
        }
    };

    // Output results
    for result in results {
        if json {
            match serde_json::to_string(&result) {
                Ok(json_str) => render::out(format_args!("{json_str}")),
                Err(e) => {
                    render::progress(format_args!("error serializing result: {e}"));
                    return RunOutcome::Usage {
                        detail: format!("{e}"),
                    };
                }
            }
        } else {
            render::out(format_args!(
                "gate={:?} passed={} duration_ms={}",
                result.kind, result.passed, result.duration_ms
            ));
        }
    }

    RunOutcome::Drained
}

fn parse_gate_kind(s: &str) -> Option<GateKind> {
    match s.to_lowercase().as_str() {
        "baseline" => Some(GateKind::Baseline),
        "targeted" => Some(GateKind::Targeted),
        "verify" => Some(GateKind::Verify),
        "lint" => Some(GateKind::Lint),
        "format" => Some(GateKind::Format),
        "build" => Some(GateKind::Build),
        "privacy" => Some(GateKind::Privacy),
        "flake" => Some(GateKind::Flake),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;

    #[test]
    fn cli_rerun_gate_with_invalid_task_id_exits_2() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), None, "not_a_number".to_string(), None, false);
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage (exit 2), got {outcome:?}"),
        }
    }

    #[test]
    fn cli_rerun_gate_with_nonexistent_task_exits_2() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), None, "999".to_string(), None, false);
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage (exit 2), got {outcome:?}"),
        }
    }

    #[test]
    fn parse_gate_kind_recognizes_all_kinds() {
        assert_eq!(parse_gate_kind("baseline"), Some(GateKind::Baseline));
        assert_eq!(parse_gate_kind("targeted"), Some(GateKind::Targeted));
        assert_eq!(parse_gate_kind("verify"), Some(GateKind::Verify));
        assert_eq!(parse_gate_kind("lint"), Some(GateKind::Lint));
        assert_eq!(parse_gate_kind("format"), Some(GateKind::Format));
        assert_eq!(parse_gate_kind("build"), Some(GateKind::Build));
        assert_eq!(parse_gate_kind("privacy"), Some(GateKind::Privacy));
        assert_eq!(parse_gate_kind("flake"), Some(GateKind::Flake));
    }

    #[test]
    fn parse_gate_kind_is_case_insensitive() {
        assert_eq!(parse_gate_kind("VERIFY"), Some(GateKind::Verify));
        assert_eq!(parse_gate_kind("Verify"), Some(GateKind::Verify));
        assert_eq!(parse_gate_kind("BUILD"), Some(GateKind::Build));
    }

    #[test]
    fn parse_gate_kind_rejects_unknown_kind() {
        assert_eq!(parse_gate_kind("unknown"), None);
        assert_eq!(parse_gate_kind("test"), None);
        assert_eq!(parse_gate_kind(""), None);
    }
}

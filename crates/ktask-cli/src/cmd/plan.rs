//! Plan command: validate tasks in the queue.

use crate::{cli::PlanSubcommand, render};
use ktask_core::{Project, RunOutcome, queue};
use std::collections::HashSet;

pub(crate) fn run(
    project: Option<Project>,
    _subcommand: PlanSubcommand,
    _json: bool,
) -> RunOutcome {
    lint(project)
}

fn lint(project: Option<Project>) -> RunOutcome {
    let Some(proj) = project else {
        return RunOutcome::Drained;
    };

    let tasks = match queue::load(&proj) {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error: {e}"));
            return RunOutcome::Usage {
                detail: e.to_string(),
            };
        }
    };

    let mut issues = Vec::new();

    // Check for duplicate IDs
    let mut seen_ids = HashSet::new();
    for task in &tasks {
        if !seen_ids.insert(task.id) {
            issues.push(format!("T{:03}: duplicate task id", task.id.get()));
        }
    }

    // Validate each task
    for task in &tasks {
        if let Err(e) = task.validate() {
            issues.push(format!("T{:03}: {}", task.id.get(), e));
        }

        // Check Verify command is not empty (additional check beyond validate())
        if task.verify.trim().is_empty() {
            issues.push(format!("T{:03}: Verify command is empty", task.id.get()));
        }
    }

    if !issues.is_empty() {
        for issue in &issues {
            render::out(format_args!("{issue}"));
        }
        return RunOutcome::Usage {
            detail: format!("{} problems found", issues.len()),
        };
    }

    RunOutcome::Drained
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{Journal, Task, TaskId, TaskStatus};
    use tempfile::TempDir;

    fn setup_test_project() -> (TempDir, Project) {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        std::process::Command::new("git")
            .arg("init")
            .current_dir(repo_path)
            .output()
            .expect("git init failed");

        let proj = ktask_core::register(repo_path).unwrap();
        (temp, proj)
    }

    fn create_task(id: u32, outcome: &str, done_when: &str, verify: &str, refs: &str) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: outcome.to_string(),
            done_when: done_when.to_string(),
            verify: verify.to_string(),
            refs: refs.to_string(),
        }
    }

    #[test]
    fn plan_lint_empty_queue() {
        let (_temp, proj) = setup_test_project();
        let outcome = lint(Some(proj));
        assert!(matches!(outcome, RunOutcome::Drained));
    }

    #[test]
    fn plan_lint_valid_queue() {
        let (_temp, proj) = setup_test_project();
        let mut journal = Journal::open_for(&proj).unwrap();

        let tasks = vec![
            create_task(1, "outcome 1", "done 1", "verify 1", "refs 1"),
            create_task(2, "outcome 2", "done 2", "verify 2", "refs 2"),
        ];
        journal.put_tasks(&tasks).unwrap();

        let outcome = lint(Some(proj));
        assert!(matches!(outcome, RunOutcome::Drained));
    }

    #[test]
    fn plan_lint_detects_missing_outcome() {
        let (_temp, proj) = setup_test_project();
        let mut journal = Journal::open_for(&proj).unwrap();

        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task 1".to_string(),
            outcome: String::new(),
            done_when: "done".to_string(),
            verify: "verify".to_string(),
            refs: "refs".to_string(),
        };
        journal.put_tasks(&[task]).unwrap();

        let outcome = lint(Some(proj));
        match outcome {
            RunOutcome::Usage { detail } => {
                assert!(detail.contains("problem"));
            }
            _ => panic!("Expected Usage outcome"),
        }
    }

    #[test]
    fn plan_lint_detects_empty_verify() {
        let (_temp, proj) = setup_test_project();
        let mut journal = Journal::open_for(&proj).unwrap();

        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task 1".to_string(),
            outcome: "outcome".to_string(),
            done_when: "done".to_string(),
            verify: String::new(),
            refs: "refs".to_string(),
        };
        journal.put_tasks(&[task]).unwrap();

        let outcome = lint(Some(proj));
        match outcome {
            RunOutcome::Usage { detail } => {
                assert!(detail.contains("problem"));
            }
            _ => panic!("Expected Usage outcome"),
        }
    }

    #[test]
    fn plan_lint_reports_all_issues() {
        let (_temp, proj) = setup_test_project();
        let mut journal = Journal::open_for(&proj).unwrap();

        let task1 = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task 1".to_string(),
            outcome: String::new(),
            done_when: "done".to_string(),
            verify: "verify".to_string(),
            refs: "refs".to_string(),
        };
        let task2 = Task {
            id: TaskId::new(2),
            status: TaskStatus::Pending,
            body: "Task 2".to_string(),
            outcome: "outcome".to_string(),
            done_when: String::new(),
            verify: "verify".to_string(),
            refs: "refs".to_string(),
        };
        journal.put_tasks(&[task1, task2]).unwrap();

        let outcome = lint(Some(proj));
        match outcome {
            RunOutcome::Usage { detail } => {
                assert!(detail.contains("2 problems"));
            }
            _ => panic!("Expected Usage outcome"),
        }
    }
}

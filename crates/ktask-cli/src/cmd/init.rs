//! Init command: register the current repository.

use crate::render;
use ktask_core::RunOutcome;
use std::path::Path;

/// Register the current repository.
///
/// Discovers the git repository and registers it, printing the project id
/// and state directory. Returns exit code 0 on success, 2 if not in a git
/// repository.
pub(crate) fn run() -> RunOutcome {
    run_with_start_path_and_state_root(Path::new("."), None)
}

/// Internal: run with explicit start path and optional state root override.
///
/// Used internally and by tests to control discovery and state directory location.
fn run_with_start_path_and_state_root(start: &Path, state_root: Option<&Path>) -> RunOutcome {
    match ktask_core::discover_with_state_root(start, state_root) {
        Ok(project) => {
            render::out(format_args!("id={}", project.id));
            render::out(format_args!("state={}", project.state_dir.display()));
            RunOutcome::Drained
        }
        Err(e) => RunOutcome::Usage {
            detail: format!("{e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_git_repo(path: &Path) {
        std::process::Command::new("git")
            .arg("init")
            .current_dir(path)
            .output()
            .expect("git init failed");
    }

    #[test]
    fn init_in_git_repo_succeeds() {
        let repo_temp = TempDir::new().unwrap();
        let repo_path = repo_temp.path();
        init_git_repo(repo_path);

        let state_temp = TempDir::new().unwrap();
        let state_root = state_temp.path();

        let outcome = run_with_start_path_and_state_root(repo_path, Some(state_root));

        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }
    }

    #[test]
    fn init_outside_git_repo_fails() {
        let non_repo_temp = TempDir::new().unwrap();
        let non_repo_path = non_repo_temp.path();

        let state_temp = TempDir::new().unwrap();
        let state_root = state_temp.path();

        let outcome = run_with_start_path_and_state_root(non_repo_path, Some(state_root));

        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage error, got {outcome:?}"),
        }
    }

    #[test]
    fn init_is_idempotent() {
        let repo_temp = TempDir::new().unwrap();
        let repo_path = repo_temp.path();
        init_git_repo(repo_path);

        let state_temp = TempDir::new().unwrap();
        let state_root = state_temp.path();

        let outcome1 = run_with_start_path_and_state_root(repo_path, Some(state_root));
        let outcome2 = run_with_start_path_and_state_root(repo_path, Some(state_root));

        match (outcome1, outcome2) {
            (RunOutcome::Drained, RunOutcome::Drained) => {}
            _ => panic!("Expected both runs to succeed"),
        }
    }
}

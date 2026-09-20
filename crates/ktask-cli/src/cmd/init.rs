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
    match ktask_core::discover(Path::new(".")) {
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
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);

        // Change to the temp directory
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(repo_path).unwrap();

        let outcome = run();

        std::env::set_current_dir(original_cwd).unwrap();

        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }
    }

    #[test]
    fn init_outside_git_repo_fails() {
        let temp = TempDir::new().unwrap();
        let non_repo_path = temp.path();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(non_repo_path).unwrap();

        let outcome = run();

        std::env::set_current_dir(original_cwd).unwrap();

        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage error, got {outcome:?}"),
        }
    }

    #[test]
    fn init_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(repo_path).unwrap();

        let outcome1 = run();
        let outcome2 = run();

        std::env::set_current_dir(original_cwd).unwrap();

        match (outcome1, outcome2) {
            (RunOutcome::Drained, RunOutcome::Drained) => {}
            _ => panic!("Expected both runs to succeed"),
        }
    }
}

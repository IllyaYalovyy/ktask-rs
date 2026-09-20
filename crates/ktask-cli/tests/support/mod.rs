//! Support helpers for scenario-based end-to-end tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Holds temporary directories and paths for a scenario run.
pub(crate) struct ScenarioEnv {
    /// Temporary directory for the entire scenario (state, repo, etc.)
    _temp_root: TempDir,
    /// Path to the git repository (scratch project).
    pub repo_dir: PathBuf,
    /// Path to the state directory (`XDG_STATE_HOME` equivalent).
    pub state_dir: PathBuf,
    /// Path to the built ktask-rs binary.
    pub binary_path: PathBuf,
}

impl ScenarioEnv {
    /// Create a new scenario environment with a temporary git repository, state directory,
    /// and configured binary path.
    ///
    /// # Panics
    ///
    /// Panics if the temporary directory cannot be created, if git init fails, or if the
    /// built binary cannot be found.
    #[allow(missing_docs)]
    pub(crate) fn new() -> Self {
        let temp_root = TempDir::new().expect("failed to create temp directory");
        let temp_path = temp_root.path();

        let repo_dir = temp_path.join("repo");
        let state_dir = temp_path.join("state");

        std::fs::create_dir_all(&repo_dir).expect("failed to create repo directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create state directory");

        init_git_repo(&repo_dir);
        create_bare_origin(&repo_dir);

        // Try to get the binary path from Cargo, fall back to searching in target/debug.
        let binary_path = if let Ok(path) = env::var("CARGO_BIN_EXE_ktask_rs") {
            PathBuf::from(path)
        } else {
            let manifest_dir =
                env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
            PathBuf::from(manifest_dir)
                .ancestors()
                .find(|p| p.join("target/debug/ktask-rs").exists())
                .map_or_else(
                    || PathBuf::from("target/debug/ktask-rs"),
                    |p| p.join("target/debug/ktask-rs"),
                )
        };

        ScenarioEnv {
            _temp_root: temp_root,
            repo_dir,
            state_dir,
            binary_path,
        }
    }

    /// Write a task file to the scenario directory and return its path.
    ///
    /// # Panics
    ///
    /// Panics if the file cannot be written.
    #[allow(missing_docs)]
    pub(crate) fn write_task(&self, name: &str, content: &str) -> PathBuf {
        let path = self.repo_dir.join(name);
        std::fs::write(&path, content).expect("failed to write task file");
        path
    }

    /// Run a command in the repository directory with `XDG_STATE_HOME` set to the state directory.
    ///
    /// # Panics
    ///
    /// Panics if the binary cannot be executed.
    #[allow(missing_docs)]
    pub(crate) fn run_command(&self, args: &[&str]) -> ScenarioOutput {
        let mut cmd = Command::new(&self.binary_path);

        cmd.current_dir(&self.repo_dir)
            .env("XDG_STATE_HOME", &self.state_dir);

        for arg in args {
            cmd.arg(arg);
        }

        let output = cmd.output().expect("failed to execute binary");

        ScenarioOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        }
    }
}

/// Output from running a command in a scenario.
#[derive(Debug, Clone)]
pub(crate) struct ScenarioOutput {
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Exit code.
    pub exit_code: i32,
}

impl ScenarioOutput {
    /// Assert that the command succeeded (exit code 0).
    #[must_use]
    pub(crate) fn expect_success(self) -> Self {
        assert_eq!(
            self.exit_code, 0,
            "expected exit code 0, got {}\nstdout: {}\nstderr: {}",
            self.exit_code, self.stdout, self.stderr
        );
        self
    }

    /// Assert that the command failed with a specific exit code.
    #[expect(dead_code)]
    #[must_use]
    pub(crate) fn expect_exit_code(self, code: i32) -> Self {
        assert_eq!(
            self.exit_code, code,
            "expected exit code {}, got {}\nstdout: {}\nstderr: {}",
            code, self.exit_code, self.stdout, self.stderr
        );
        self
    }

    /// Assert that stdout contains a substring.
    pub(crate) fn assert_stdout_contains(&self, substring: &str) {
        assert!(
            self.stdout.contains(substring),
            "stdout does not contain '{}'\nstdout: {}",
            substring,
            self.stdout
        );
    }

    /// Assert that stderr contains a substring.
    #[expect(dead_code)]
    pub(crate) fn assert_stderr_contains(&self, substring: &str) {
        assert!(
            self.stderr.contains(substring),
            "stderr does not contain '{}'\nstderr: {}",
            substring,
            self.stderr
        );
    }
}

/// Initialize a git repository at the given path.
fn init_git_repo(path: &Path) {
    let output = Command::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .expect("failed to execute git init");

    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Create a bare origin remote for the repository.
fn create_bare_origin(repo_dir: &Path) {
    let bare_dir = repo_dir.parent().unwrap().join("origin");
    std::fs::create_dir_all(&bare_dir).expect("failed to create bare directory");

    let output = Command::new("git")
        .arg("init")
        .arg("--bare")
        .current_dir(&bare_dir)
        .output()
        .expect("failed to execute git init --bare");

    assert!(
        output.status.success(),
        "git init --bare failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new("git")
        .arg("remote")
        .arg("add")
        .arg("origin")
        .arg(bare_dir.to_string_lossy().as_ref())
        .current_dir(repo_dir)
        .output()
        .expect("failed to add remote");

    assert!(
        output.status.success(),
        "git remote add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

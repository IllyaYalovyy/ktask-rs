//! Git command wrapper for subprocess execution.

use std::path::Path;
use std::process::Command;

use crate::{Error, Result};

/// Execute a git command and return trimmed stdout.
///
/// Runs git as a subprocess with the given arguments in the specified root
/// directory. Returns the trimmed standard output if successful. Non-zero exit
/// codes are converted to [`Error::Git`] carrying the command arguments and
/// stderr output.
///
/// # Arguments
///
/// * `root` - The working directory for git execution
/// * `args` - Git command arguments (excluding the 'git' binary itself)
///
/// # Errors
///
/// Returns an error if:
/// - The git command cannot be spawned
/// - Git exits with a non-zero status code
///
/// # Examples
///
/// ```no_run
/// # use std::path::Path;
/// # use ktask_core::git;
/// let root = Path::new(".");
/// let output = git(root, &["status"]);
/// ```
pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root);

    let output = cmd.output().map_err(|e| Error::Gate {
        kind: "Git".to_string(),
        detail: format!("Failed to spawn git command: {e}"),
    })?;

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        return Err(Error::Git {
            args: args.iter().map(|s| s.to_string()).collect(),
            stderr,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    fn temp_git_repo() -> PathBuf {
        let tmp = env::temp_dir().join(format!("ktask-git-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("Failed to create temp directory");

        // Initialize a git repo
        Command::new("git")
            .args(&["init"])
            .current_dir(&tmp)
            .output()
            .expect("Failed to init git repo");

        // Configure git user
        Command::new("git")
            .args(&["config", "user.email", "test@example.com"])
            .current_dir(&tmp)
            .output()
            .expect("Failed to configure user.email");

        Command::new("git")
            .args(&["config", "user.name", "Test User"])
            .current_dir(&tmp)
            .output()
            .expect("Failed to configure user.name");

        tmp
    }

    #[test]
    fn git_status_returns_output() {
        let repo = temp_git_repo();
        let result = git(&repo, &["status"]);
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("On branch"));
    }

    #[test]
    fn git_with_nonexistent_command_fails() {
        let repo = temp_git_repo();
        let result = git(&repo, &["nonexistent-command"]);
        assert!(result.is_err());
        if let Err(Error::Git { args, stderr }) = result {
            assert_eq!(args, vec!["nonexistent-command"]);
            assert!(!stderr.is_empty());
        } else {
            panic!("Expected Error::Git variant");
        }
    }

    #[test]
    fn git_error_carries_arguments() {
        let repo = temp_git_repo();
        let result = git(&repo, &["nonexistent"]);
        assert!(result.is_err());
        if let Err(Error::Git { args, .. }) = result {
            assert!(args.contains(&"nonexistent".to_string()));
        } else {
            panic!("Expected Error::Git variant");
        }
    }

    #[test]
    fn git_trims_output() {
        let repo = temp_git_repo();
        let result = git(&repo, &["status"]);
        assert!(result.is_ok());
        let output = result.unwrap();
        // Output should be trimmed
        assert!(!output.starts_with(' '));
        assert!(!output.starts_with('\n'));
        assert!(!output.ends_with(' '));
        assert!(!output.ends_with('\n'));
    }

    #[test]
    fn git_error_variant_has_stderr() {
        let repo = temp_git_repo();
        let result = git(&repo, &["invalid-flag-xyzabc"]);
        assert!(result.is_err());
        if let Err(Error::Git { args, stderr }) = result {
            assert!(!stderr.is_empty(), "stderr should contain error message");
            assert!(args.contains(&"invalid-flag-xyzabc".to_string()));
        } else {
            panic!("Expected Error::Git variant");
        }
    }
}

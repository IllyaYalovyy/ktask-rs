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
            args: args.iter().map(ToString::to_string).collect(),
            stderr,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(stdout.trim().to_string())
}

/// Get the SHA of the current HEAD commit.
///
/// # Errors
///
/// Returns an error if the git command fails or if HEAD does not exist.
pub fn head_sha(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "HEAD"])
}

/// Get the name of the current branch.
///
/// # Errors
///
/// Returns an error if the git command fails or if HEAD does not exist.
pub fn current_branch(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Get the URL of the remote repository.
///
/// # Errors
///
/// Returns an error if the git command fails or if the remote does not exist.
pub fn remote_url(root: &Path) -> Result<String> {
    git(root, &["config", "--get", "remote.origin.url"])
}

/// Get the porcelain status of the repository.
///
/// Returns the output of `git status --porcelain`, which shows all modified,
/// staged, and untracked files (except ignored ones).
///
/// # Errors
///
/// Returns an error if the git command fails.
pub fn status_porcelain(root: &Path) -> Result<String> {
    git(root, &["status", "--porcelain"])
}

/// Check if the repository is clean.
///
/// Returns true if there are no modified, staged, or untracked files.
/// Ignored files are not considered.
///
/// # Errors
///
/// Returns an error if the git command fails.
pub fn is_clean(root: &Path) -> Result<bool> {
    let status = status_porcelain(root)?;
    Ok(status.is_empty())
}

/// Fetch from a remote repository.
///
/// # Errors
///
/// Returns an error if the git command fails.
pub fn fetch(root: &Path, remote: &str) -> Result<()> {
    git(root, &["fetch", remote])?;
    Ok(())
}

/// Create a new worktree at the given base SHA.
///
/// Creates a new git worktree detached at the specified commit SHA. The worktree
/// is created in a subdirectory named `name` under the repository root.
///
/// # Arguments
///
/// * `root` - The root directory of the git repository
/// * `name` - The name of the worktree (used as subdirectory name)
/// * `base_sha` - The commit SHA to check out in the worktree
///
/// # Errors
///
/// Returns an error if the git command fails or the SHA is invalid.
pub fn create_worktree(root: &Path, name: &str, base_sha: &str) -> Result<std::path::PathBuf> {
    let worktree_path = root.join(name);
    git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            worktree_path.to_str().unwrap_or(""),
            base_sha,
        ],
    )?;
    Ok(worktree_path)
}

/// Remove a worktree.
///
/// Removes a git worktree, cleaning up all associated data. The worktree directory
/// itself is removed.
///
/// # Arguments
///
/// * `root` - The root directory of the git repository
/// * `name` - The name of the worktree to remove
///
/// # Errors
///
/// Returns an error if the git command fails or the worktree does not exist.
pub fn remove_worktree(root: &Path, name: &str) -> Result<()> {
    git(root, &["worktree", "remove", name])?;
    Ok(())
}

/// List all worktrees in the repository.
///
/// Returns the names of all worktrees excluding the main working directory.
/// Each line of the output contains the worktree path and metadata.
///
/// # Arguments
///
/// * `root` - The root directory of the git repository
///
/// # Errors
///
/// Returns an error if the git command fails.
pub fn list_worktrees(root: &Path) -> Result<Vec<String>> {
    let root_canonical = root.canonicalize().ok();
    let output = git(root, &["worktree", "list"])?;
    let worktrees: Vec<String> = output
        .lines()
        .filter_map(|line| {
            let path = line.split_whitespace().next()?;
            if let Ok(p) = Path::new(path).canonicalize() {
                if let Some(root_c) = &root_canonical
                    && &p == root_c
                {
                    return None; // Skip main worktree
                }
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(ToString::to_string)
            } else {
                None
            }
        })
        .collect();
    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    fn temp_git_repo() -> Option<PathBuf> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());

        let tmp = env::temp_dir().join(format!(
            "ktask-git-test-{}-{}",
            std::process::id(),
            timestamp
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).ok()?;

        // Initialize a git repo
        let init_output = Command::new("git")
            .args(["init"])
            .current_dir(&tmp)
            .output()
            .ok()?;

        if !init_output.status.success() {
            let _ = fs::remove_dir_all(&tmp);
            return None;
        }

        // Configure git user
        let email_output = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&tmp)
            .output()
            .ok()?;

        if !email_output.status.success() {
            let _ = fs::remove_dir_all(&tmp);
            return None;
        }

        let name_output = Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(&tmp)
            .output()
            .ok()?;

        if !name_output.status.success() {
            let _ = fs::remove_dir_all(&tmp);
            return None;
        }

        Some(tmp)
    }

    #[test]
    fn git_status_returns_output() {
        let Some(repo) = temp_git_repo() else {
            return; // Skip if git is not available
        };
        let result = git(&repo, &["status"]);
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("On branch"));
    }

    #[test]
    fn git_with_nonexistent_command_fails() {
        let Some(repo) = temp_git_repo() else {
            return; // Skip if git is not available
        };
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
        let Some(repo) = temp_git_repo() else {
            return; // Skip if git is not available
        };
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
        let Some(repo) = temp_git_repo() else {
            return; // Skip if git is not available
        };
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
        let Some(repo) = temp_git_repo() else {
            return; // Skip if git is not available
        };
        let result = git(&repo, &["invalid-flag-xyzabc"]);
        assert!(result.is_err());
        if let Err(Error::Git { args, stderr }) = result {
            assert!(!stderr.is_empty(), "stderr should contain error message");
            assert!(args.contains(&"invalid-flag-xyzabc".to_string()));
        } else {
            panic!("Expected Error::Git variant");
        }
    }

    #[test]
    fn head_sha_returns_commit_hash() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create a commit
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let result = head_sha(&repo);
        assert!(result.is_ok());
        let sha = result.unwrap();
        assert_eq!(sha.len(), 40); // SHA-1 is 40 hex characters
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn current_branch_returns_branch_name() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create a commit so HEAD exists
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let result = current_branch(&repo);
        assert!(result.is_ok());
        let branch = result.unwrap();
        // On a fresh repo, this is usually "master" or "main"
        assert!(!branch.is_empty());
    }

    #[test]
    fn remote_url_returns_error_when_no_remote() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Newly initialized repo has no origin remote
        let result = remote_url(&repo);
        assert!(result.is_err());
    }

    #[test]
    fn status_porcelain_returns_empty_when_clean() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create and commit a file
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let result = status_porcelain(&repo);
        assert!(result.is_ok());
        let status = result.unwrap();
        assert!(status.is_empty());
    }

    #[test]
    fn status_porcelain_shows_modified_files() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create and commit a file
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        // Modify the file
        fs::write(&file_path, "modified content").ok();

        let result = status_porcelain(&repo);
        assert!(result.is_ok());
        let status = result.unwrap();
        assert!(!status.is_empty());
        assert!(status.contains("test.txt"));
    }

    #[test]
    fn status_porcelain_shows_untracked_files() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create and commit a file
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        // Create an untracked file
        let untracked_path = repo.join("untracked.txt");
        fs::write(&untracked_path, "untracked content").ok();

        let result = status_porcelain(&repo);
        assert!(result.is_ok());
        let status = result.unwrap();
        assert!(!status.is_empty());
        assert!(status.contains("untracked.txt"));
    }

    #[test]
    fn status_porcelain_ignores_ignored_files() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create and commit a file
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        let commit_status = Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output();
        if commit_status.is_err() {
            return;
        }

        // Create a .gitignore file
        let gitignore_path = repo.join(".gitignore");
        fs::write(&gitignore_path, "*.log\n").ok();
        Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&repo)
            .output()
            .ok();
        let gitignore_commit = Command::new("git")
            .args(["commit", "-m", "add gitignore"])
            .current_dir(&repo)
            .output();
        if gitignore_commit.is_err() {
            return;
        }

        // Create an ignored file
        let ignored_path = repo.join("debug.log");
        fs::write(&ignored_path, "log content").ok();

        let result = status_porcelain(&repo);
        assert!(result.is_ok(), "status_porcelain should succeed");
        let status = result.unwrap();
        // Ignored files should not appear in status
        assert!(
            !status.contains("debug.log"),
            "ignored file 'debug.log' should not appear in status, but got: {status:?}"
        );
    }

    #[test]
    fn is_clean_returns_true_when_clean() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create and commit a file
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let result = is_clean(&repo);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn is_clean_returns_false_when_modified() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create and commit a file
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        // Modify the file
        fs::write(&file_path, "modified content").ok();

        let result = is_clean(&repo);
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn is_clean_returns_false_when_untracked() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create and commit a file
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        // Create an untracked file
        let untracked_path = repo.join("untracked.txt");
        fs::write(&untracked_path, "untracked content").ok();

        let result = is_clean(&repo);
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn is_clean_ignores_ignored_files() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create and commit a file
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        let commit1 = Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output();
        if commit1.is_err() {
            return;
        }

        // Create a .gitignore file
        let gitignore_path = repo.join(".gitignore");
        fs::write(&gitignore_path, "*.log\n").ok();
        Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&repo)
            .output()
            .ok();
        let commit2 = Command::new("git")
            .args(["commit", "-m", "add gitignore"])
            .current_dir(&repo)
            .output();
        if commit2.is_err() {
            return;
        }

        // Create an ignored file
        let ignored_path = repo.join("debug.log");
        fs::write(&ignored_path, "log content").ok();

        let result = is_clean(&repo);
        assert!(result.is_ok());
        assert!(
            result.unwrap(),
            "is_clean should return true when only ignored files are present"
        );
    }

    #[test]
    fn fetch_succeeds_with_local_remote() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create a bare repository to act as a remote
        let remote_path = env::temp_dir().join(format!("ktask-git-remote-{}", std::process::id()));
        let _ = fs::remove_dir_all(&remote_path);
        let _ = fs::create_dir_all(&remote_path);

        // Initialize as bare repo
        if Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&remote_path)
            .output()
            .is_err()
        {
            return; // Skip if we can't create the remote
        }

        // Add the remote to our test repo
        let _ = Command::new("git")
            .args([
                "remote",
                "add",
                "test-remote",
                remote_path.to_str().unwrap(),
            ])
            .current_dir(&repo)
            .output();

        // Create and push a commit
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();
        let _ = Command::new("git")
            .args(["push", "test-remote", "HEAD"])
            .current_dir(&repo)
            .output();

        // Now test fetch
        let result = fetch(&repo, "test-remote");
        assert!(result.is_ok());

        // Clean up remote
        let _ = fs::remove_dir_all(&remote_path);
    }

    #[test]
    fn create_worktree_creates_detached_worktree() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create a commit
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let sha_result = head_sha(&repo);
        let Some(sha) = sha_result.ok() else {
            return;
        };

        // Create a worktree at the commit
        let result = create_worktree(&repo, "test-worktree", &sha);
        assert!(result.is_ok());
        let worktree_path = result.unwrap();

        // Verify the worktree exists
        assert!(worktree_path.exists());
        assert!(worktree_path.join(".git").exists());

        // Check that it's at the correct commit
        let worktree_sha_result = head_sha(&worktree_path);
        assert!(worktree_sha_result.is_ok());
        assert_eq!(worktree_sha_result.unwrap(), sha);

        // Clean up
        let _ = fs::remove_dir_all(&worktree_path);
    }

    #[test]
    fn create_worktree_respects_given_sha() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create first commit
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "content 1").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "commit 1"])
            .current_dir(&repo)
            .output()
            .ok();

        let first_sha = head_sha(&repo).unwrap();

        // Create second commit
        fs::write(&file_path, "content 2").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "commit 2"])
            .current_dir(&repo)
            .output()
            .ok();

        let second_sha = head_sha(&repo).unwrap();

        // Create a worktree at the first commit
        let result = create_worktree(&repo, "test-worktree-1", &first_sha);
        assert!(result.is_ok());
        let worktree_path = result.unwrap();

        // Verify the worktree is at the first commit, not the current HEAD
        let worktree_sha = head_sha(&worktree_path).unwrap();
        assert_eq!(worktree_sha, first_sha);
        assert_ne!(worktree_sha, second_sha);

        // Clean up
        let _ = fs::remove_dir_all(&worktree_path);
    }

    #[test]
    fn remove_worktree_deletes_worktree() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create a commit
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let sha = head_sha(&repo).unwrap();

        // Create a worktree
        let worktree_path = create_worktree(&repo, "test-worktree", &sha).unwrap();
        assert!(worktree_path.exists());

        // Remove the worktree
        let result = remove_worktree(&repo, "test-worktree");
        assert!(result.is_ok());

        // Verify it's gone
        assert!(!worktree_path.exists());
    }

    #[test]
    fn list_worktrees_shows_created_worktrees() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create a commit
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let sha = head_sha(&repo).unwrap();

        // Clean up any leftover worktrees first
        let existing = list_worktrees(&repo).unwrap();
        for wt in existing {
            let _ = remove_worktree(&repo, &wt);
        }

        // Now list should be empty
        let initial_list = list_worktrees(&repo).unwrap();
        assert!(initial_list.is_empty());

        // Create a worktree
        let _worktree_path = create_worktree(&repo, "test-worktree", &sha).unwrap();

        // List should now contain the worktree
        let list = list_worktrees(&repo).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&"test-worktree".to_string()));

        // Clean up
        let _ = remove_worktree(&repo, "test-worktree");
    }

    #[test]
    fn list_worktrees_detects_multiple_worktrees() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create a commit
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let sha = head_sha(&repo).unwrap();

        // Clean up any leftover worktrees first
        let existing = list_worktrees(&repo).unwrap();
        for wt in existing {
            let _ = remove_worktree(&repo, &wt);
        }

        // Create multiple worktrees
        let _wt1 = create_worktree(&repo, "wt1", &sha).unwrap();
        let _wt2 = create_worktree(&repo, "wt2", &sha).unwrap();

        // List should contain both
        let list = list_worktrees(&repo).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.contains(&"wt1".to_string()));
        assert!(list.contains(&"wt2".to_string()));

        // Clean up
        let _ = remove_worktree(&repo, "wt1");
        let _ = remove_worktree(&repo, "wt2");
    }

    #[test]
    fn list_worktrees_detects_leftover_worktree() {
        let Some(repo) = temp_git_repo() else {
            return;
        };
        // Create a commit
        let file_path = repo.join("test.txt");
        fs::write(&file_path, "test content").ok();
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(&repo)
            .output()
            .ok();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .ok();

        let sha = head_sha(&repo).unwrap();

        // Create a worktree
        let worktree_path = create_worktree(&repo, "leftover-wt", &sha).unwrap();

        // List should show it
        let list_before = list_worktrees(&repo).unwrap();
        assert!(list_before.contains(&"leftover-wt".to_string()));

        // Remove the directory manually (simulating leftover)
        let _ = fs::remove_dir_all(&worktree_path);

        // Clean up git's reference
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&repo)
            .output();

        // List should no longer show it after prune
        let list_after = list_worktrees(&repo).unwrap();
        assert!(!list_after.contains(&"leftover-wt".to_string()));
    }
}

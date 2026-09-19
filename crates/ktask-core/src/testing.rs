//! Disposable repository fixtures for testing.

#[cfg(any(test, feature = "testing"))]
use crate::{Error, Result};
#[cfg(any(test, feature = "testing"))]
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "testing"))]
use std::process::Command;

/// A temporary git repository for testing.
///
/// Created in the system temp directory with a bare origin remote and a seed
/// commit. The repository is automatically cleaned up when dropped.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug)]
pub struct ScratchRepo {
    /// Working directory of the repository.
    pub work: tempfile::TempDir,
    /// Path to the bare origin remote.
    pub origin: PathBuf,
}

#[cfg(any(test, feature = "testing"))]
impl ScratchRepo {
    /// Create a new scratch repository with a seed commit.
    ///
    /// The seed commit is made with fixed author, email, and deterministic
    /// timestamps so its hash is reproducible.
    ///
    /// # Errors
    ///
    /// Returns an error if the repository cannot be initialized or if git
    /// commands fail.
    pub fn new() -> Result<Self> {
        // Create temporary directories
        let work = tempfile::TempDir::new().map_err(|e| Error::Gate {
            kind: "TempDir".to_string(),
            detail: format!("Failed to create temp directory: {e}"),
        })?;

        let origin = std::env::temp_dir().join(format!(
            "ktask-test-origin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&origin).map_err(|e| Error::Gate {
            kind: "TempDir".to_string(),
            detail: format!("Failed to create origin directory: {e}"),
        })?;

        // Initialize bare origin
        git_with_config(
            &origin,
            &["init", "--bare"],
            &[],
        )?;

        // Initialize working repo
        git_with_config(
            work.path(),
            &["init"],
            &[],
        )?;

        // Add origin remote
        git_with_config(
            work.path(),
            &["remote", "add", "origin", origin.to_str().unwrap()],
            &[],
        )?;

        // Create seed commit
        let test_file = work.path().join("seed.txt");
        std::fs::write(&test_file, "seed content").map_err(|e| Error::Gate {
            kind: "FileWrite".to_string(),
            detail: format!("Failed to write seed file: {e}"),
        })?;

        git_with_config(
            work.path(),
            &["add", "seed.txt"],
            &[],
        )?;

        // Use fixed timestamps for deterministic commit hash
        let env = vec![
            ("GIT_AUTHOR_DATE", "2000-01-01 00:00:00 +0000"),
            ("GIT_COMMITTER_DATE", "2000-01-01 00:00:00 +0000"),
        ];

        git_with_env(
            work.path(),
            &["commit", "-m", "seed commit"],
            &[],
            &env,
        )?;

        Ok(ScratchRepo { work, origin })
    }

    /// Get the path to the working directory.
    pub fn path(&self) -> &Path {
        self.work.path()
    }

    /// Get the path to the origin remote.
    pub fn origin_path(&self) -> &Path {
        &self.origin
    }

    /// Create a new commit with the given message.
    ///
    /// # Errors
    ///
    /// Returns an error if the git command fails.
    pub fn commit(&self, message: &str) -> Result<String> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let test_file = self.path().join(format!("file_{}.txt", timestamp));
        std::fs::write(&test_file, "content").map_err(|e| Error::Gate {
            kind: "FileWrite".to_string(),
            detail: format!("Failed to write test file: {e}"),
        })?;

        let filename = test_file.file_name().unwrap().to_str().unwrap();
        git_with_config(
            self.path(),
            &["add", filename],
            &[],
        )?;

        let env = vec![
            ("GIT_AUTHOR_DATE", "2000-01-01 00:00:00 +0000"),
            ("GIT_COMMITTER_DATE", "2000-01-01 00:00:00 +0000"),
        ];

        git_with_env(
            self.path(),
            &["commit", "-m", message],
            &[],
            &env,
        )?;

        // Get the commit hash
        git_with_config(self.path(), &["rev-parse", "HEAD"], &[])
    }

    /// Create a new branch.
    ///
    /// # Errors
    ///
    /// Returns an error if the git command fails.
    pub fn branch(&self, name: &str) -> Result<()> {
        git_with_config(self.path(), &["branch", name], &[])?;
        Ok(())
    }

    /// Checkout a branch.
    ///
    /// # Errors
    ///
    /// Returns an error if the git command fails.
    pub fn checkout(&self, name: &str) -> Result<()> {
        git_with_config(self.path(), &["checkout", name], &[])?;
        Ok(())
    }

    /// Create a divergent history by committing on a different branch.
    ///
    /// Returns the commit hash of the new commit.
    ///
    /// # Errors
    ///
    /// Returns an error if the git command fails.
    pub fn diverge(&self, branch_name: &str, message: &str) -> Result<String> {
        self.branch(branch_name)?;
        self.checkout(branch_name)?;
        self.commit(message)
    }

    /// Push to origin.
    ///
    /// # Errors
    ///
    /// Returns an error if the git command fails.
    pub fn push(&self, remote: &str, refspec: &str) -> Result<()> {
        git_with_config(self.path(), &["push", remote, refspec], &[])?;
        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
impl Drop for ScratchRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.origin);
    }
}

/// Execute a git command with user configuration.
#[cfg(any(test, feature = "testing"))]
fn git_with_config(root: &Path, args: &[&str], _extra_config: &[(&str, &str)]) -> Result<String> {
    let mut config_args: Vec<String> = vec![
        "-c".to_string(),
        "user.name=Test User".to_string(),
        "-c".to_string(),
        "user.email=test@example.com".to_string(),
    ];

    config_args.extend(args.iter().map(|s| s.to_string()));

    let config_strs: Vec<&str> = config_args.iter().map(|s| s.as_str()).collect();

    let mut cmd = Command::new("git");
    cmd.args(&config_strs).current_dir(root);

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

/// Execute a git command with environment variables.
#[cfg(any(test, feature = "testing"))]
fn git_with_env(
    root: &Path,
    args: &[&str],
    extra_config: &[(&str, &str)],
    env_vars: &[(&str, &str)],
) -> Result<String> {
    let mut config_args: Vec<String> = vec![
        "-c".to_string(),
        "user.name=Test User".to_string(),
        "-c".to_string(),
        "user.email=test@example.com".to_string(),
    ];

    for (key, value) in extra_config {
        config_args.push("-c".to_string());
        config_args.push(format!("{}={}", key, value));
    }

    config_args.extend(args.iter().map(|s| s.to_string()));

    let mut cmd = Command::new("git");
    cmd.args(&config_args).current_dir(root);

    for (key, value) in env_vars {
        cmd.env(key, value);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_repo_creates_independent_instances() -> Result<()> {
        let repo1 = ScratchRepo::new()?;
        let repo2 = ScratchRepo::new()?;

        // They should have different paths
        assert_ne!(repo1.path(), repo2.path());
        assert_ne!(repo1.origin_path(), repo2.origin_path());

        // Both should exist
        assert!(repo1.path().exists());
        assert!(repo2.path().exists());

        Ok(())
    }

    #[test]
    fn scratch_repo_seed_commit_is_deterministic() -> Result<()> {
        let repo1 = ScratchRepo::new()?;
        let repo2 = ScratchRepo::new()?;

        let hash1 = crate::git::head_sha(repo1.path())?;
        let hash2 = crate::git::head_sha(repo2.path())?;

        // Seed commits should have the same hash (deterministic)
        assert_eq!(hash1, hash2);

        Ok(())
    }

    #[test]
    fn scratch_repo_cleanup_removes_everything() -> Result<()> {
        let work_path;
        let origin_path;

        {
            let repo = ScratchRepo::new()?;
            work_path = repo.path().to_path_buf();
            origin_path = repo.origin_path().to_path_buf();

            assert!(work_path.exists());
            assert!(origin_path.exists());
        }
        // repo dropped here

        assert!(!work_path.exists(), "work directory should be cleaned up");
        assert!(!origin_path.exists(), "origin directory should be cleaned up");

        Ok(())
    }

    #[test]
    fn scratch_repo_can_create_commits() -> Result<()> {
        let repo = ScratchRepo::new()?;

        let hash1 = crate::git::head_sha(repo.path())?;
        let hash2 = repo.commit("test commit")?;

        // New commit should have different hash
        assert_ne!(hash1, hash2);
        assert_eq!(hash2.len(), 40);

        Ok(())
    }

    #[test]
    fn scratch_repo_can_create_branches() -> Result<()> {
        let repo = ScratchRepo::new()?;

        repo.branch("feature")?;
        repo.checkout("feature")?;

        let branch = crate::git::current_branch(repo.path())?;
        assert_eq!(branch, "feature");

        Ok(())
    }

    #[test]
    fn scratch_repo_can_diverge() -> Result<()> {
        let repo = ScratchRepo::new()?;

        let seed_hash = crate::git::head_sha(repo.path())?;
        let diverge_hash = repo.diverge("feature", "divergent commit")?;

        // Should have different hashes
        assert_ne!(seed_hash, diverge_hash);

        // Should be on feature branch
        let branch = crate::git::current_branch(repo.path())?;
        assert_eq!(branch, "feature");

        Ok(())
    }

    #[test]
    fn scratch_repo_can_push_to_origin() -> Result<()> {
        let repo = ScratchRepo::new()?;

        repo.push("origin", "HEAD:refs/heads/main")?;

        // Verify the push by checking what's in origin
        let origin_refs = git_with_config(repo.origin_path(), &["show-ref"], &[])?;
        assert!(origin_refs.contains("main"));

        Ok(())
    }
}

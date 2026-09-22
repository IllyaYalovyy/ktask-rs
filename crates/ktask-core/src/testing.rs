//! Disposable git repositories for tests: real `git` subprocesses, no
//! network, and a seed commit whose hash is the same on every machine and
//! every run.
//!
//! [`scratch_repo`] is the entry point. It never touches this repository or
//! the machine's global git identity: every fixture lives under the system
//! temp directory, and every invocation carries its own `user.name` and
//! `user.email` via `-c`.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::Result;

const AUTHOR_NAME: &str = "ktask fixture";
const AUTHOR_EMAIL: &str = "fixture@ktask.invalid";
const SEED_DATE: &str = "2024-01-01T00:00:00+00:00";
const DEFAULT_BRANCH: &str = "main";

/// Runs `git` in `root` with the fixture's fixed identity attached to every
/// invocation via `-c`, so no fixture behavior depends on the machine's
/// global git config.
fn git(root: &Path, args: &[&str]) -> Result<String> {
    git_with_env(root, args, &[])
}

/// Like [`git`], additionally setting `env` on the subprocess — used to pin
/// `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` on the seed commit.
fn git_with_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let name = format!("user.name={AUTHOR_NAME}");
    let email = format!("user.email={AUTHOR_EMAIL}");
    let mut full: Vec<&str> = vec!["-c", &name, "-c", &email];
    full.extend_from_slice(args);
    crate::git::with_env(root, &full, env)
}

/// A disposable local repository for git-layer tests: a working checkout
/// with `origin` already configured as its remote, a bare `origin` it can
/// push to and fetch from, and one seed commit already pushed to both.
///
/// Dropping a `ScratchRepo` removes its temporary directory — checkout,
/// origin and all — from disk.
#[derive(Debug)]
pub struct ScratchRepo {
    _root: TempDir,
    /// The working checkout's path.
    pub path: PathBuf,
    /// The bare repository `path`'s `origin` remote points at.
    pub origin: PathBuf,
    /// The seed commit's full SHA. Identical across every `ScratchRepo`,
    /// since it is made with a fixed author, message and timestamp.
    pub seed_sha: String,
}

/// The result of [`ScratchRepo::diverge`]: two commits that share the seed
/// commit as their most recent common ancestor, neither an ancestor of the
/// other.
#[derive(Debug, Clone)]
pub struct Diverged {
    /// The commit left in the working checkout; never pushed to `origin`.
    pub local_sha: String,
    /// The commit pushed to `origin`; absent from the working checkout.
    pub origin_sha: String,
}

/// Builds a fresh [`ScratchRepo`] under the system temp directory.
///
/// Each call gets its own temporary directory, so fixtures never share
/// state, and never depend on the machine's global git identity or the
/// network.
///
/// # Errors
///
/// Returns [`crate::Error`] if the temp directory cannot be created or any
/// `git` invocation fails.
pub fn scratch_repo() -> Result<ScratchRepo> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("repo");
    let origin = root.path().join("origin.git");
    fs::create_dir(&path)?;
    fs::create_dir(&origin)?;

    git(&path, &["init", "--quiet"])?;
    git(
        &path,
        &[
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{DEFAULT_BRANCH}"),
        ],
    )?;
    git(&origin, &["init", "--quiet", "--bare"])?;
    git(
        &origin,
        &[
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{DEFAULT_BRANCH}"),
        ],
    )?;
    git(
        &path,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    )?;

    fs::write(path.join("SEED.md"), "ktask scratch repo seed\n")?;
    git(&path, &["add", "SEED.md"])?;
    git_with_env(
        &path,
        &["commit", "--quiet", "-m", "seed"],
        &[
            ("GIT_AUTHOR_DATE", SEED_DATE),
            ("GIT_COMMITTER_DATE", SEED_DATE),
        ],
    )?;
    let seed_sha = git(&path, &["rev-parse", "HEAD"])?;
    git(
        &path,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("HEAD:refs/heads/{DEFAULT_BRANCH}"),
        ],
    )?;

    Ok(ScratchRepo {
        _root: root,
        path,
        origin,
        seed_sha,
    })
}

impl ScratchRepo {
    /// Writes `name` with `contents` in the working checkout, stages it, and
    /// commits it under the fixture's fixed identity. Returns the new
    /// commit's full SHA.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error`] if the write or either `git` invocation
    /// fails.
    pub fn commit(&self, name: &str, contents: &str) -> Result<String> {
        fs::write(self.path.join(name), contents)?;
        git(&self.path, &["add", name])?;
        git(
            &self.path,
            &["commit", "--quiet", "-m", &format!("add {name}")],
        )?;
        git(&self.path, &["rev-parse", "HEAD"])
    }

    /// Creates and checks out a new branch named `name` from the working
    /// checkout's current `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error`] if `name` already exists or the checkout
    /// fails.
    pub fn branch(&self, name: &str) -> Result<()> {
        git(&self.path, &["checkout", "--quiet", "-b", name])?;
        Ok(())
    }

    /// Makes the working checkout and `origin` diverge: a commit lands only
    /// in the working checkout, and a different commit — made through a
    /// separate, throwaway clone — lands only on `origin`'s default branch.
    /// A plain `git push` from the working checkout after this is rejected
    /// as non-fast-forward, matching what a real collaborator pushing
    /// upstream first would produce.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error`] if any of the commits, the clone or the push
    /// fails.
    pub fn diverge(&self) -> Result<Diverged> {
        let local_sha = self.commit("local-only.txt", "local change\n")?;

        let shadow = tempfile::tempdir()?;
        git(
            shadow.path(),
            &["clone", "--quiet", &self.origin.to_string_lossy(), "."],
        )?;
        fs::write(shadow.path().join("origin-only.txt"), "origin change\n")?;
        git(shadow.path(), &["add", "origin-only.txt"])?;
        git(
            shadow.path(),
            &["commit", "--quiet", "-m", "add origin-only.txt"],
        )?;
        git(
            shadow.path(),
            &[
                "push",
                "--quiet",
                "origin",
                &format!("HEAD:refs/heads/{DEFAULT_BRANCH}"),
            ],
        )?;
        let origin_sha = git(shadow.path(), &["rev-parse", "HEAD"])?;

        Ok(Diverged {
            local_sha,
            origin_sha,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_repo_contains_an_initialized_checkout_and_a_bare_origin() {
        let repo = scratch_repo().expect("scratch_repo");

        assert!(repo.path.join(".git").is_dir());
        assert!(repo.origin.join("HEAD").is_file());
        assert!(
            !repo.origin.join(".git").exists(),
            "origin must be bare, not a checkout"
        );
    }

    #[test]
    fn the_seed_commit_is_already_on_origin() {
        let repo = scratch_repo().expect("scratch_repo");

        let origin_head = git(&repo.origin, &["rev-parse", DEFAULT_BRANCH]).expect("origin head");

        assert_eq!(origin_head, repo.seed_sha);
    }

    #[test]
    fn the_seed_commit_hash_is_deterministic() {
        let first = scratch_repo().expect("scratch_repo");
        let second = scratch_repo().expect("scratch_repo");

        assert_eq!(first.seed_sha, second.seed_sha);
    }

    #[test]
    fn two_fixtures_do_not_share_a_directory() {
        let first = scratch_repo().expect("scratch_repo");
        let second = scratch_repo().expect("scratch_repo");

        assert_ne!(first.path, second.path);
        assert_ne!(first.origin, second.origin);
    }

    #[test]
    fn a_commit_made_in_one_fixture_is_invisible_in_the_other() {
        let first = scratch_repo().expect("scratch_repo");
        let second = scratch_repo().expect("scratch_repo");

        first
            .commit("only-in-first.txt", "hello\n")
            .expect("commit");

        assert!(first.path.join("only-in-first.txt").exists());
        assert!(!second.path.join("only-in-first.txt").exists());
    }

    #[test]
    fn dropping_a_scratch_repo_removes_its_directory() {
        let repo = scratch_repo().expect("scratch_repo");
        let path = repo.path.clone();
        assert!(path.exists());

        drop(repo);

        assert!(!path.exists(), "teardown must remove the fixture's files");
    }

    #[test]
    fn commit_advances_head_and_is_visible_on_disk() {
        let repo = scratch_repo().expect("scratch_repo");

        let sha = repo.commit("file.txt", "hello\n").expect("commit");

        assert_ne!(sha, repo.seed_sha);
        assert_eq!(
            fs::read_to_string(repo.path.join("file.txt")).unwrap(),
            "hello\n"
        );
        let head = git(&repo.path, &["rev-parse", "HEAD"]).expect("rev-parse");
        assert_eq!(head, sha);
    }

    #[test]
    fn branch_creates_and_checks_out_a_new_branch() {
        let repo = scratch_repo().expect("scratch_repo");

        repo.branch("feature-x").expect("branch");

        let current = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]).expect("branch name");
        assert_eq!(current, "feature-x");
    }

    #[test]
    fn diverge_leaves_neither_commit_an_ancestor_of_the_other() {
        let repo = scratch_repo().expect("scratch_repo");

        let diverged = repo.diverge().expect("diverge");

        assert_ne!(diverged.local_sha, diverged.origin_sha);
        let local_contains_origin = git(
            &repo.path,
            &[
                "merge-base",
                "--is-ancestor",
                &diverged.origin_sha,
                &diverged.local_sha,
            ],
        );
        assert!(
            local_contains_origin.is_err(),
            "origin's commit must not be an ancestor of the local commit"
        );
    }

    #[test]
    fn diverge_makes_a_plain_push_rejected_as_non_fast_forward() {
        let repo = scratch_repo().expect("scratch_repo");
        repo.diverge().expect("diverge");

        let err = git(
            &repo.path,
            &[
                "push",
                "origin",
                &format!("HEAD:refs/heads/{DEFAULT_BRANCH}"),
            ],
        )
        .expect_err("a diverged push must be rejected");

        let message = err.to_string();
        assert!(
            message.contains("fetch first") || message.contains("non-fast-forward"),
            "expected a non-fast-forward rejection, got {message:?}"
        );
    }
}

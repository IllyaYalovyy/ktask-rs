//! `git`, run as a subprocess: the one place in ktask-core that shells out to
//! the `git` binary, so every caller gets the same argv-only invocation and
//! the same error shape (`docs/DESIGN.md` Dependencies).
//!
//! Every other module that needs git talks to this one function rather than
//! building its own [`std::process::Command`], so there is exactly one place
//! that can get shell-quoting or error handling wrong.

use std::path::Path;
use std::process::Command;

use crate::{Error, Result};

/// Runs `git` with `args` in `root`, returning its trimmed standard output.
///
/// `args` becomes the process's argv directly — never a shell string, so
/// there is nothing in a path or commit message for a shell to reinterpret.
///
/// # Errors
///
/// Returns [`Error::Git`] naming `args` and carrying `git`'s stderr when the
/// process could not be spawned (`git` not on `PATH`, a nonexistent `root`)
/// or exited non-zero.
pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    with_env(root, args, &[])
}

/// Like [`git`], but also sets `env` on the subprocess.
///
/// Only the crate's scratch-repository test fixtures need this: pinning
/// `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` is what makes a seed commit's hash
/// reproducible across machines and runs. Every other caller goes through
/// [`git`] so this stays the one function that actually spawns the process.
pub(crate) fn with_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(root);
    for (key, value) in env {
        command.env(key, value);
    }

    let output = command.output().map_err(|err| Error::Git {
        args: args_owned(args),
        stderr: err.to_string(),
    })?;

    if !output.status.success() {
        return Err(Error::Git {
            args: args_owned(args),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Copies `args` into an owned `Vec<String>` for [`Error::Git`], which must
/// outlive the borrowed `&[&str]` a caller passed in.
fn args_owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

/// Returns the full SHA of the commit `HEAD` points at in `root`.
///
/// # Errors
///
/// Returns [`Error::Git`] if `root` has no commits yet or is not a git
/// repository.
pub fn head_sha(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "HEAD"])
}

/// Returns the name of the branch currently checked out in `root`.
///
/// Returns the literal string `"HEAD"` when `root` is in a detached-HEAD
/// state, matching `git rev-parse --abbrev-ref HEAD`.
///
/// # Errors
///
/// Returns [`Error::Git`] if `root` is not a git repository.
pub fn current_branch(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Returns the configured fetch URL for `remote` in `root`.
///
/// # Errors
///
/// Returns [`Error::Git`] if `remote` is not configured in `root`.
pub fn remote_url(root: &Path, remote: &str) -> Result<String> {
    git(root, &["remote", "get-url", remote])
}

/// Returns `git status --porcelain` output for `root`: one line per changed
/// or untracked path, empty when the working tree matches `HEAD`.
///
/// Ignored files are omitted, as `git status` omits them by default.
///
/// # Errors
///
/// Returns [`Error::Git`] if `root` is not a git repository.
pub fn status_porcelain(root: &Path) -> Result<String> {
    git(root, &["status", "--porcelain"])
}

/// Reports whether `root`'s working tree and index match `HEAD`.
///
/// Ignored files never count against cleanliness; untracked files do, since
/// [`status_porcelain`] lists them.
///
/// # Errors
///
/// Returns [`Error::Git`] if `root` is not a git repository.
pub fn is_clean(root: &Path) -> Result<bool> {
    Ok(status_porcelain(root)?.is_empty())
}

/// Fetches `remote` into `root`, updating its remote-tracking refs.
///
/// # Errors
///
/// Returns [`Error::Git`] if `remote` is not configured or unreachable.
pub fn fetch(root: &Path, remote: &str) -> Result<()> {
    git(root, &["fetch", remote]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init"]).expect("git init");
        git(dir.path(), &["config", "user.email", "test@example.com"]).expect("config email");
        git(dir.path(), &["config", "user.name", "Test"]).expect("config name");
        // Pin the initial branch name so tests don't depend on the
        // environment's `init.defaultBranch`.
        git(dir.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]).expect("pin branch");
        dir
    }

    fn commit_file(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).expect("write file");
        git(dir, &["add", name]).expect("git add");
        git(dir, &["commit", "-m", &format!("add {name}")]).expect("git commit");
    }

    #[test]
    fn returns_trimmed_stdout_on_success() {
        let dir = init_repo();

        let output = git(dir.path(), &["rev-parse", "--is-inside-work-tree"]).expect("git");

        assert_eq!(output, "true");
    }

    #[test]
    fn a_failing_command_names_the_arguments_in_the_error() {
        let dir = init_repo();

        let err = git(dir.path(), &["show", "no-such-ref-at-all"]).expect_err("must fail");

        let Error::Git { args, stderr } = &err else {
            panic!("expected Error::Git, got {err:?}");
        };
        assert_eq!(args, &["show", "no-such-ref-at-all"]);
        assert!(!stderr.is_empty(), "stderr should carry git's own message");
        let message = err.to_string();
        assert!(message.contains("show"));
        assert!(message.contains("no-such-ref-at-all"));
    }

    #[test]
    fn a_nonexistent_root_is_a_clear_error_not_a_panic() {
        let err = git(Path::new("/no/such/ktask-git-test-root"), &["status"])
            .expect_err("must fail cleanly");

        assert!(matches!(&err, Error::Git { args, .. } if args == &["status"]));
    }

    #[test]
    fn output_is_trimmed_of_trailing_newline() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");

        let output = git(dir.path(), &["log", "--format=%s", "-1"]).expect("git log");

        assert_eq!(output, "add file.txt");
        assert!(!output.ends_with('\n'));
    }

    #[test]
    fn head_sha_matches_the_commit_git_reports() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");

        let sha = head_sha(dir.path()).expect("head_sha");
        let expected = git(dir.path(), &["rev-parse", "HEAD"]).expect("rev-parse");

        assert_eq!(sha, expected);
        assert_eq!(sha.len(), 40, "expected a full SHA, got {sha:?}");
    }

    #[test]
    fn head_sha_fails_before_the_first_commit() {
        let dir = init_repo();

        let err = head_sha(dir.path()).expect_err("must fail: no commits yet");

        assert!(matches!(err, Error::Git { .. }));
    }

    #[test]
    fn current_branch_returns_the_checked_out_branch_name() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        git(dir.path(), &["checkout", "-b", "feature-x"]).expect("checkout -b");

        let branch = current_branch(dir.path()).expect("current_branch");

        assert_eq!(branch, "feature-x");
    }

    #[test]
    fn current_branch_reports_head_when_detached() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        let sha = head_sha(dir.path()).expect("head_sha");
        git(dir.path(), &["checkout", &sha]).expect("detach HEAD");

        let branch = current_branch(dir.path()).expect("current_branch");

        assert_eq!(branch, "HEAD");
    }

    #[test]
    fn remote_url_returns_the_configured_url() {
        let dir = init_repo();
        git(
            dir.path(),
            &["remote", "add", "origin", "https://example.com/repo.git"],
        )
        .expect("remote add");

        let url = remote_url(dir.path(), "origin").expect("remote_url");

        assert_eq!(url, "https://example.com/repo.git");
    }

    #[test]
    fn remote_url_fails_for_an_unconfigured_remote() {
        let dir = init_repo();

        let err = remote_url(dir.path(), "origin").expect_err("must fail: no such remote");

        assert!(matches!(err, Error::Git { .. }));
    }

    #[test]
    fn status_porcelain_is_empty_for_a_clean_tree() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");

        let status = status_porcelain(dir.path()).expect("status_porcelain");

        assert_eq!(status, "");
    }

    #[test]
    fn status_porcelain_lists_an_untracked_file() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("new.txt"), "new\n").expect("write file");

        let status = status_porcelain(dir.path()).expect("status_porcelain");

        assert!(
            status.contains("new.txt"),
            "expected new.txt in status, got {status:?}"
        );
    }

    #[test]
    fn is_clean_is_true_for_a_freshly_committed_tree() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");

        assert!(is_clean(dir.path()).expect("is_clean"));
    }

    #[test]
    fn is_clean_is_false_for_an_untracked_file() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("untracked.txt"), "new\n").expect("write file");

        assert!(!is_clean(dir.path()).expect("is_clean"));
    }

    #[test]
    fn is_clean_is_false_for_a_modified_tracked_file() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("file.txt"), "changed\n").expect("write file");

        assert!(!is_clean(dir.path()).expect("is_clean"));
    }

    #[test]
    fn is_clean_ignores_files_matched_by_gitignore() {
        let dir = init_repo();
        commit_file(dir.path(), ".gitignore", "ignored.txt\n");
        std::fs::write(dir.path().join("ignored.txt"), "should not count\n").expect("write file");

        assert!(
            is_clean(dir.path()).expect("is_clean"),
            "an ignored file must not count as dirty"
        );
    }

    #[test]
    fn fetch_updates_remote_tracking_refs() {
        let origin = init_repo();
        commit_file(origin.path(), "file.txt", "hello\n");
        let expected_sha = head_sha(origin.path()).expect("head_sha");

        let local = init_repo();
        git(
            local.path(),
            &[
                "remote",
                "add",
                "origin",
                origin.path().to_str().expect("utf8 path"),
            ],
        )
        .expect("remote add");

        fetch(local.path(), "origin").expect("fetch");

        let fetched_sha = git(local.path(), &["rev-parse", "refs/remotes/origin/main"])
            .expect("rev-parse remote-tracking ref");
        assert_eq!(fetched_sha, expected_sha);
    }

    #[test]
    fn fetch_fails_for_an_unconfigured_remote() {
        let dir = init_repo();

        let err = fetch(dir.path(), "origin").expect_err("must fail: no such remote");

        assert!(matches!(err, Error::Git { .. }));
    }
}

//! `git`, run as a subprocess: the one place in ktask-core that shells out to
//! the `git` binary, so every caller gets the same argv-only invocation and
//! the same error shape (`docs/DESIGN.md` Dependencies).
//!
//! Every other module that needs git talks to this one function rather than
//! building its own [`std::process::Command`], so there is exactly one place
//! that can get shell-quoting or error handling wrong.

use std::path::{Path, PathBuf};
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

/// One entry from [`list_worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The worktree's checkout directory.
    pub path: PathBuf,
    /// The full SHA the worktree's `HEAD` points at.
    pub head: String,
    /// The branch checked out in the worktree, `None` when detached.
    pub branch: Option<String>,
    /// Whether git considers this entry prunable: its administrative data
    /// survives in `root`'s `.git` directory, but the checkout it names is
    /// gone (removed by hand, or never finished by an interrupted run).
    pub prunable: bool,
}

/// Creates (or reclaims) an isolated task worktree checked out at `base_sha`,
/// and returns its path.
///
/// The worktree lives under this project's private state directory, keyed by
/// `name`, never inside `root` or beside it: the caller's own checkout is
/// never touched (VISION.md §10). It is created from `base_sha` directly —
/// never from whatever `root` currently has checked out — so the candidate a
/// task works from is exactly the commit the caller named.
///
/// Calling this again with the same `root` and `name` reuses the existing
/// worktree as-is rather than failing or recreating it, so a task resuming
/// after a restart gets back the worktree (and any work already committed in
/// it) it left behind. A *prunable* leftover — git's administrative record
/// for a worktree whose directory is gone — is reclaimed instead: removed,
/// then recreated fresh at `base_sha`.
///
/// # Errors
///
/// Returns [`Error::Git`] if `base_sha` does not exist in `root` or the
/// worktree cannot be created, [`Error::Io`] if the state directory cannot
/// be created, and [`Error::Config`] naming `HOME` if neither
/// `XDG_STATE_HOME` nor `HOME` is set.
pub fn create_worktree(root: &Path, name: &str, base_sha: &str) -> Result<PathBuf> {
    create_worktree_with(root, name, base_sha, &|key| std::env::var(key).ok())
}

fn create_worktree_with(
    root: &Path,
    name: &str,
    base_sha: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf> {
    let dest = worktree_path(root, name, env)?;

    match list_worktrees(root)?.into_iter().find(|w| w.path == dest) {
        Some(existing) if existing.prunable => remove_worktree(root, &dest)?,
        Some(_) => return Ok(dest),
        None => {}
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dest_arg = dest.to_string_lossy();
    git(
        root,
        &["worktree", "add", "--detach", dest_arg.as_ref(), base_sha],
    )?;
    Ok(dest)
}

/// The path `create_worktree` uses for `name`: under this project's private
/// state directory (never inside or beside `root`), namespaced by
/// [`crate::paths::project_id`] so distinct projects never collide.
fn worktree_path(root: &Path, name: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    let id = crate::paths::project_id(root, None);
    let state = crate::paths::state_root_with(env)?;
    Ok(state.join(id).join("worktrees").join(name))
}

/// Removes the worktree at `path` from `root`, discarding any uncommitted
/// changes in it.
///
/// Also reclaims a *prunable* entry — one whose directory is already gone —
/// by dropping its administrative record from `root`'s `.git` directory.
/// Succeeds whether or not `path` currently exists on disk, so it is safe to
/// call on a worktree a previous run left behind in either state.
///
/// # Errors
///
/// Returns [`Error::Git`] if `root` has no worktree registered at `path`.
pub fn remove_worktree(root: &Path, path: &Path) -> Result<()> {
    let path_arg = path.to_string_lossy();
    git(root, &["worktree", "remove", "--force", path_arg.as_ref()])?;
    Ok(())
}

/// Lists every worktree `root` knows about, including its own primary
/// checkout and any prunable leftover from an interrupted run.
///
/// # Errors
///
/// Returns [`Error::Git`] if `root` is not a git repository.
pub fn list_worktrees(root: &Path) -> Result<Vec<Worktree>> {
    let output = git(root, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_list(&output))
}

/// Parses `git worktree list --porcelain` output into [`Worktree`] entries.
///
/// Entries are blank-line-separated blocks of `key value` lines; unrecognized
/// keys (`locked`, bare `detached`, lock/prune reasons) are ignored, since
/// only path, head and branch are needed by callers today.
fn parse_worktree_list(output: &str) -> Vec<Worktree> {
    output
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut path = PathBuf::new();
            let mut head = String::new();
            let mut branch = None;
            let mut prunable = false;
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("worktree ") {
                    path = PathBuf::from(rest);
                } else if let Some(rest) = line.strip_prefix("HEAD ") {
                    head = rest.to_string();
                } else if let Some(rest) = line.strip_prefix("branch ") {
                    branch = Some(rest.to_string());
                } else if line == "prunable" || line.starts_with("prunable ") {
                    prunable = true;
                }
            }
            Worktree {
                path,
                head,
                branch,
                prunable,
            }
        })
        .collect()
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

    fn env_with_state_home(dir: &tempfile::TempDir) -> impl Fn(&str) -> Option<String> {
        let home = dir.path().join("state").to_string_lossy().to_string();
        move |key| (key == "XDG_STATE_HOME").then(|| home.clone())
    }

    #[test]
    fn parse_worktree_list_reads_path_head_and_branch() {
        let output = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\n\
                       worktree /repo/.worktrees/task-1\nHEAD def456\ndetached";

        let entries = parse_worktree_list(output);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, Path::new("/repo"));
        assert_eq!(entries[0].head, "abc123");
        assert_eq!(entries[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(!entries[0].prunable);
        assert_eq!(entries[1].path, Path::new("/repo/.worktrees/task-1"));
        assert_eq!(entries[1].branch, None);
        assert!(!entries[1].prunable);
    }

    #[test]
    fn parse_worktree_list_flags_a_prunable_entry() {
        let output = "worktree /repo/.worktrees/gone\nHEAD abc123\ndetached\n\
                       prunable gitdir file points to non-existent location";

        let entries = parse_worktree_list(output);

        assert_eq!(entries.len(), 1);
        assert!(entries[0].prunable);
    }

    #[test]
    fn create_worktree_checks_out_base_sha_not_the_current_checkout() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let base_sha = repo.seed_sha.clone();
        repo.commit("later.txt", "later\n").expect("advance HEAD");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);

        let dest =
            create_worktree_with(&repo.path, "task-1", &base_sha, &env).expect("create_worktree");

        assert!(dest.join("SEED.md").is_file());
        assert!(
            !dest.join("later.txt").exists(),
            "worktree must be built from base_sha, not root's current checkout"
        );
        assert_eq!(head_sha(&dest).expect("head_sha"), base_sha);
        assert_eq!(
            current_branch(&dest).expect("current_branch"),
            "HEAD",
            "checkout must be detached at base_sha, not on a branch"
        );
    }

    #[test]
    fn create_worktree_reuses_an_existing_worktree_for_the_same_name() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);

        let first = create_worktree_with(&repo.path, "task-1", &repo.seed_sha, &env)
            .expect("first create_worktree");
        std::fs::write(first.join("scratch.txt"), "keep me\n").expect("write marker file");

        let second = create_worktree_with(&repo.path, "task-1", &repo.seed_sha, &env)
            .expect("second create_worktree");

        assert_eq!(first, second);
        assert!(
            second.join("scratch.txt").is_file(),
            "reusing an existing worktree must not recreate it"
        );
        let entries = list_worktrees(&repo.path).expect("list_worktrees");
        assert_eq!(entries.iter().filter(|w| w.path == first).count(), 1);
    }

    #[test]
    fn remove_worktree_drops_it_from_list_worktrees_and_from_disk() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);
        let dest = create_worktree_with(&repo.path, "task-1", &repo.seed_sha, &env)
            .expect("create_worktree");

        remove_worktree(&repo.path, &dest).expect("remove_worktree");

        assert!(!dest.exists());
        let entries = list_worktrees(&repo.path).expect("list_worktrees");
        assert!(!entries.iter().any(|w| w.path == dest));
    }

    #[test]
    fn list_worktrees_flags_a_leftover_directory_as_prunable() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);
        let dest = create_worktree_with(&repo.path, "task-1", &repo.seed_sha, &env)
            .expect("create_worktree");
        std::fs::remove_dir_all(&dest)
            .expect("simulate a crash: directory gone, admin record left behind");

        let entries = list_worktrees(&repo.path).expect("list_worktrees");

        let leftover = entries
            .iter()
            .find(|w| w.path == dest)
            .expect("leftover worktree is still listed");
        assert!(leftover.prunable);
    }

    #[test]
    fn create_worktree_reclaims_a_prunable_leftover_from_a_previous_run() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);
        let dest = create_worktree_with(&repo.path, "task-1", &repo.seed_sha, &env)
            .expect("first create_worktree");
        std::fs::remove_dir_all(&dest).expect("simulate a crash");

        let reclaimed = create_worktree_with(&repo.path, "task-1", &repo.seed_sha, &env)
            .expect("create_worktree must reclaim the prunable leftover");

        assert_eq!(reclaimed, dest);
        assert!(
            dest.join("SEED.md").is_file(),
            "reclaimed worktree must be a real checkout again"
        );
        let entries = list_worktrees(&repo.path).expect("list_worktrees");
        assert_eq!(entries.iter().filter(|w| w.path == dest).count(), 1);
        assert!(!entries.iter().any(|w| w.path == dest && w.prunable));
    }
}

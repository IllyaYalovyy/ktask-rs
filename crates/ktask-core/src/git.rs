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

/// Fails if `worktree`'s working tree or index differs from `HEAD`: a dirty
/// tree at verification time is a policy failure (`VISION.md` §10).
///
/// Ignored files never trigger this, since `git ls-files --others
/// --exclude-standard` omits them.
///
/// Deliberately uses `git diff --cached`, `git diff` and `git ls-files
/// --others` rather than parsing `status_porcelain`'s output: porcelain's
/// leading status column can itself be a space (an unstaged-only change
/// reads as `" M path"`), and [`git`] trims the subprocess's stdout, which
/// would eat exactly that leading space when it opens the very first line —
/// misreading an unstaged modification as staged.
///
/// # Errors
///
/// Returns [`Error::Policy`] if `worktree` is not clean. `detail` groups the
/// offending paths by how they are dirty — staged, modified or untracked, a
/// path appearing under more than one when it is both staged and further
/// modified — and `paths` names every offending path, not merely a count.
/// Returns [`Error::Git`] if `worktree` is not a git repository.
pub fn require_clean(worktree: &Path) -> Result<()> {
    let staged = name_only_paths(worktree, &["diff", "--cached", "--name-only"])?;
    let modified = name_only_paths(worktree, &["diff", "--name-only"])?;
    let untracked = name_only_paths(worktree, &["ls-files", "--others", "--exclude-standard"])?;

    if staged.is_empty() && modified.is_empty() && untracked.is_empty() {
        return Ok(());
    }

    let mut sections = Vec::new();
    if !staged.is_empty() {
        sections.push(format!("staged: {}", format_paths(&staged)));
    }
    if !modified.is_empty() {
        sections.push(format!("modified: {}", format_paths(&modified)));
    }
    if !untracked.is_empty() {
        sections.push(format!("untracked: {}", format_paths(&untracked)));
    }

    let mut paths: Vec<PathBuf> = staged
        .into_iter()
        .chain(modified)
        .chain(untracked)
        .collect();
    paths.sort();
    paths.dedup();

    Err(Error::Policy {
        detail: format!("dirty working tree: {}", sections.join("; ")),
        paths,
    })
}

/// Runs `git` with `args` in `worktree` and splits its trimmed stdout into
/// one [`PathBuf`] per line, the shape `--name-only` and `ls-files` output
/// share. Empty stdout (nothing to report) yields an empty `Vec`.
fn name_only_paths(worktree: &Path, args: &[&str]) -> Result<Vec<PathBuf>> {
    let output = git(worktree, args)?;
    Ok(output.lines().map(PathBuf::from).collect())
}

/// Renders `paths` as a comma-separated list for [`require_clean`]'s error detail.
fn format_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Stages every tracked change in `worktree` and commits it with `message`,
/// returning the new commit's SHA (`HEAD` after the commit, matching
/// [`head_sha`]).
///
/// Only tracked changes are staged (`git add -u`); an untracked file on its
/// own is not swept in and, with nothing else staged, produces
/// [`Error::NothingToCommit`] rather than a surprise commit.
///
/// # Errors
///
/// Returns [`Error::NothingToCommit`] if `worktree` has nothing staged after
/// tracked changes are added — calling this again right after a successful
/// commit, with nothing further changed, is this error rather than an empty
/// commit.
///
/// Returns [`Error::Git`] if `worktree` is not a git repository or the
/// commit itself fails.
pub fn commit_all(worktree: &Path, message: &str) -> Result<String> {
    git(worktree, &["add", "-u"])?;

    if name_only_paths(worktree, &["diff", "--cached", "--name-only"])?.is_empty() {
        return Err(Error::NothingToCommit {
            worktree: worktree.to_path_buf(),
        });
    }

    git(worktree, &["commit", "-m", message])?;
    head_sha(worktree)
}

/// Fetches `remote` into `root`, updating its remote-tracking refs.
///
/// # Errors
///
/// Returns [`Error::Git`] if `remote` is not configured or unreachable.
pub fn fetch(root: &Path, remote: &str) -> Result<()> {
    git(root, &["fetch", remote]).map(|_| ())
}

/// Pushes `candidate` to `branch` on `remote` from `worktree`, then fetches
/// `remote` again and requires the freshly fetched tip of `branch` to equal
/// `candidate` — proving publication landed rather than merely attempting it
/// (VISION.md §10).
///
/// Pushes `candidate:refs/heads/branch` rather than `HEAD:refs/heads/branch`,
/// so this works from a worktree whose `HEAD` is detached at `candidate`
/// (the shape [`create_worktree`] leaves it in).
///
/// The post-push check re-fetches rather than trusting the push command's
/// own exit status: it re-reads `refs/remotes/{remote}/{branch}` only after
/// a fresh [`fetch`], never a remote-tracking ref left over from an earlier
/// call, so a tip moved by another actor between the push and this check is
/// still caught.
///
/// # Errors
///
/// Returns [`Error::Git`] if the push itself is rejected by `remote` —
/// `args[0]` is `"push"` and `stderr` carries `git`'s own rejection message,
/// unedited.
///
/// Returns [`Error::Git`] if the push is accepted but the freshly fetched
/// tip of `branch` does not equal `candidate`: `args[0]` is `"publish"`, and
/// `stderr` names both `candidate` and the fetched SHA, so this is
/// distinguishable from a rejected push both by `args` and by message.
pub fn publish(worktree: &Path, remote: &str, branch: &str, candidate: &str) -> Result<()> {
    let refspec = format!("{candidate}:refs/heads/{branch}");
    git(worktree, &["push", remote, &refspec])?;

    fetch(worktree, remote)?;
    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    let fetched = git(worktree, &["rev-parse", &remote_ref])?;

    if fetched != candidate {
        return Err(Error::Git {
            args: vec![
                "publish".to_string(),
                "compare".to_string(),
                remote.to_string(),
                branch.to_string(),
            ],
            stderr: format!(
                "published tip mismatch on {remote}/{branch}: candidate {candidate} but freshly fetched remote tip is {fetched}"
            ),
        });
    }

    Ok(())
}

/// The result of [`rebase_onto_remote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// The rebase completed cleanly. `new_sha` is the candidate's new tip,
    /// replayed on top of the fetched remote branch.
    Applied {
        /// The full SHA of the rebased candidate's new `HEAD`.
        new_sha: String,
    },
    /// The rebase hit a conflict and was aborted; `worktree` is back exactly
    /// as it was before this call.
    Conflict {
        /// Every path git reported as conflicted (`UU` in `git status`),
        /// not merely a count.
        paths: Vec<PathBuf>,
    },
}

/// Fetches `remote` into `worktree`, then replays `worktree`'s `HEAD` onto
/// the freshly fetched tip of `branch` — the mechanical repair for a
/// rejected push caused by branch drift (`VISION.md` §7, `git_conflict`).
///
/// A clean divergence — changes that touch disjoint paths on each side —
/// fast-replays without touching a human or an agent; a conflicting one
/// aborts the rebase immediately, leaving `worktree` exactly as it was
/// before this call, and reports every conflicted path so a caller can
/// decide what happens next.
///
/// # Errors
///
/// Returns [`Error::Git`] if `remote` cannot be fetched, or if `git rebase`
/// fails for a reason other than a conflict (for example, `worktree` itself
/// is dirty going in) — that failure is not [`RebaseOutcome::Conflict`],
/// since no conflicted path exists to report.
pub fn rebase_onto_remote(worktree: &Path, remote: &str, branch: &str) -> Result<RebaseOutcome> {
    fetch(worktree, remote)?;
    let remote_ref = format!("refs/remotes/{remote}/{branch}");

    if let Err(err) = git(worktree, &["rebase", &remote_ref]) {
        let conflicted = name_only_paths(worktree, &["diff", "--name-only", "--diff-filter=U"])?;
        // `git rebase --abort` only runs while a rebase is actually in
        // progress; running it unconditionally on an unrelated failure would
        // itself fail and mask the real error.
        if conflicted.is_empty() {
            return Err(err);
        }
        git(worktree, &["rebase", "--abort"])?;
        return Ok(RebaseOutcome::Conflict { paths: conflicted });
    }

    Ok(RebaseOutcome::Applied {
        new_sha: head_sha(worktree)?,
    })
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

/// Lists every path that differs between `base` and `worktree`'s `HEAD` —
/// added, modified, deleted and renamed files alike (`VISION.md` §9, §10:
/// callers use this to see what a task actually changed).
///
/// Rename detection is requested explicitly (`--find-renames`) rather than
/// relying on the ambient `diff.renames` config, so a renamed file with
/// unchanged content is reported once, under its new path, regardless of the
/// machine's git configuration.
///
/// # Errors
///
/// Returns [`Error::Git`] if `base` does not resolve in `worktree`, or
/// `worktree` is not a git repository.
pub fn changed_paths(worktree: &Path, base: &str) -> Result<Vec<PathBuf>> {
    name_only_paths(
        worktree,
        &["diff", "--name-only", "--find-renames", base, "HEAD"],
    )
}

/// Returns `git diff --stat` between `base` and `worktree`'s `HEAD`: one
/// summary line per changed path plus the trailing totals line, exactly as
/// `git` renders it.
///
/// A changed binary file is reported by name only ("Bin NN -> MM bytes"),
/// never its contents, since `--stat` never renders binary content.
///
/// # Errors
///
/// Returns [`Error::Git`] if `base` does not resolve in `worktree`, or
/// `worktree` is not a git repository.
pub fn diff_summary(worktree: &Path, base: &str) -> Result<String> {
    git(
        worktree,
        &["diff", "--stat", "--find-renames", base, "HEAD"],
    )
}

/// Returns the full unified diff of `path` between `base` and `worktree`'s
/// `HEAD`.
///
/// A binary file is reported as `git` itself reports it — "Binary files ...
/// differ" — never its contents, since `git diff` never renders binary
/// content without `--text`, which this never passes.
///
/// # Errors
///
/// Returns [`Error::Git`] if `base` does not resolve in `worktree`, or
/// `worktree` is not a git repository.
pub fn file_diff(worktree: &Path, base: &str, path: &Path) -> Result<String> {
    let path_arg = path.to_string_lossy();
    git(
        worktree,
        &[
            "diff",
            "--find-renames",
            base,
            "HEAD",
            "--",
            path_arg.as_ref(),
        ],
    )
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
    fn require_clean_passes_for_a_freshly_committed_tree() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");

        require_clean(dir.path()).expect("require_clean");
    }

    #[test]
    fn require_clean_ignores_files_matched_by_gitignore() {
        let dir = init_repo();
        commit_file(dir.path(), ".gitignore", "ignored.txt\n");
        std::fs::write(dir.path().join("ignored.txt"), "should not count\n").expect("write file");

        require_clean(dir.path()).expect("an ignored file must never trigger require_clean");
    }

    #[test]
    fn require_clean_names_an_untracked_path_as_untracked() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("new.txt"), "new\n").expect("write file");

        let err = require_clean(dir.path()).expect_err("must fail: untracked file");

        let Error::Policy { detail, paths } = &err else {
            panic!("expected Error::Policy, got {err:?}");
        };
        assert_eq!(paths, &[PathBuf::from("new.txt")]);
        assert!(
            detail.contains("untracked"),
            "detail must name the untracked category, got {detail:?}"
        );
        assert!(
            detail.contains("new.txt"),
            "detail must name the path, got {detail:?}"
        );
    }

    #[test]
    fn require_clean_names_a_modified_tracked_path_as_modified() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("file.txt"), "changed\n").expect("write file");

        let err = require_clean(dir.path()).expect_err("must fail: unstaged modification");

        let Error::Policy { detail, paths } = &err else {
            panic!("expected Error::Policy, got {err:?}");
        };
        assert_eq!(paths, &[PathBuf::from("file.txt")]);
        assert!(
            detail.contains("modified") && !detail.contains("untracked"),
            "detail must name the modified category, got {detail:?}"
        );
    }

    #[test]
    fn require_clean_names_a_staged_path_as_staged() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("file.txt"), "changed\n").expect("write file");
        git(dir.path(), &["add", "file.txt"]).expect("git add");

        let err = require_clean(dir.path()).expect_err("must fail: staged modification");

        let Error::Policy { detail, paths } = &err else {
            panic!("expected Error::Policy, got {err:?}");
        };
        assert_eq!(paths, &[PathBuf::from("file.txt")]);
        assert!(
            detail.contains("staged") && !detail.contains("modified:"),
            "detail must name the staged category, not the modified one, got {detail:?}"
        );
    }

    #[test]
    fn require_clean_distinguishes_staged_from_further_modified_in_the_same_path() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("file.txt"), "staged change\n").expect("write file");
        git(dir.path(), &["add", "file.txt"]).expect("git add");
        std::fs::write(dir.path().join("file.txt"), "further unstaged change\n")
            .expect("write file");

        let err = require_clean(dir.path()).expect_err("must fail: staged and modified");

        let Error::Policy { detail, paths } = &err else {
            panic!("expected Error::Policy, got {err:?}");
        };
        assert_eq!(paths, &[PathBuf::from("file.txt")]);
        assert!(detail.contains("staged"), "got {detail:?}");
        assert!(detail.contains("modified"), "got {detail:?}");
    }

    #[test]
    fn require_clean_names_every_offending_path_not_just_the_count() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("file.txt"), "changed\n").expect("write file");
        std::fs::write(dir.path().join("new.txt"), "new\n").expect("write file");
        std::fs::write(dir.path().join("also-new.txt"), "also new\n").expect("write file");

        let err = require_clean(dir.path()).expect_err("must fail: three dirty paths");

        let Error::Policy { paths, .. } = &err else {
            panic!("expected Error::Policy, got {err:?}");
        };
        assert_eq!(
            paths.len(),
            3,
            "expected all three paths named, got {paths:?}"
        );
        assert!(paths.contains(&PathBuf::from("file.txt")));
        assert!(paths.contains(&PathBuf::from("new.txt")));
        assert!(paths.contains(&PathBuf::from("also-new.txt")));
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
    fn commit_all_stages_tracked_changes_and_returns_the_new_head_sha() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("file.txt"), "changed\n").expect("write file");

        let sha = commit_all(dir.path(), "update file").expect("commit_all");

        assert_eq!(sha, head_sha(dir.path()).expect("head_sha"));
        assert_eq!(
            git(dir.path(), &["log", "--format=%s", "-1"]).expect("git log"),
            "update file"
        );
    }

    #[test]
    fn commit_all_twice_with_no_changes_is_an_error_not_an_empty_commit() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("file.txt"), "changed\n").expect("write file");
        commit_all(dir.path(), "first commit").expect("commit_all");
        let sha_after_first = head_sha(dir.path()).expect("head_sha");

        let err = commit_all(dir.path(), "second commit").expect_err("must fail: nothing staged");

        assert!(matches!(err, Error::NothingToCommit { .. }));
        assert_eq!(
            head_sha(dir.path()).expect("head_sha"),
            sha_after_first,
            "HEAD must not move when there is nothing to commit"
        );
    }

    #[test]
    fn commit_all_does_not_stage_untracked_files() {
        let dir = init_repo();
        commit_file(dir.path(), "file.txt", "hello\n");
        std::fs::write(dir.path().join("new.txt"), "new\n").expect("write file");

        let err =
            commit_all(dir.path(), "should fail").expect_err("must fail: only untracked changes");

        assert!(matches!(err, Error::NothingToCommit { .. }));
        assert!(
            !status_porcelain(dir.path())
                .expect("status_porcelain")
                .is_empty(),
            "untracked file must remain untouched, not silently committed"
        );
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
    fn publish_succeeds_when_the_fetched_remote_tip_matches_the_candidate() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let candidate = repo.commit("file.txt", "hello\n").expect("commit");

        publish(&repo.path, "origin", "main", &candidate).expect("publish");

        let remote_tip =
            git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse on bare origin");
        assert_eq!(remote_tip, candidate);
    }

    #[test]
    fn publish_surfaces_a_rejected_push_distinctly_from_a_mismatch() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let diverged = repo
            .diverge()
            .expect("diverge: local and origin now disagree");

        let err = publish(&repo.path, "origin", "main", &diverged.local_sha)
            .expect_err("must fail: non-fast-forward push");

        let Error::Git { args, stderr } = &err else {
            panic!("expected Error::Git, got {err:?}");
        };
        assert_eq!(
            args[0], "push",
            "a rejected push must surface git's own push failure verbatim, not our comparison"
        );
        assert!(
            !stderr.contains("mismatch"),
            "a rejected push must not be worded like a fetched-tip mismatch, got {stderr:?}"
        );
    }

    #[test]
    fn publish_detects_a_remote_tip_moved_between_push_and_a_fresh_fetch() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let stale_sha = repo.seed_sha.clone();

        // A post-receive hook on the bare origin resets `main` back to its
        // pre-push tip the instant the push is accepted — standing in for
        // another actor racing in between the push landing and this
        // function's own re-fetch. `publish` must catch this because it
        // re-fetches rather than trusting the push's own success.
        let hook_path = repo.origin.join("hooks").join("post-receive");
        std::fs::write(
            &hook_path,
            format!("#!/bin/sh\ngit update-ref refs/heads/main {stale_sha}\n"),
        )
        .expect("write post-receive hook");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path)
                .expect("hook metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).expect("chmod hook");
        }

        let candidate = repo.commit("advance.txt", "advance\n").expect("commit");

        let err =
            publish(&repo.path, "origin", "main", &candidate).expect_err("must fail: mismatch");

        let Error::Git { args, stderr } = &err else {
            panic!("expected Error::Git, got {err:?}");
        };
        assert_eq!(
            args[0], "publish",
            "a fetched-tip mismatch must not be reported as a push failure"
        );
        assert!(
            stderr.contains(&candidate),
            "stderr must name the candidate sha, got {stderr:?}"
        );
        assert!(
            stderr.contains(&stale_sha),
            "stderr must name the freshly fetched sha, got {stderr:?}"
        );
    }

    #[test]
    fn rebase_onto_remote_applies_a_clean_divergence_and_reports_the_new_sha() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let diverged = repo.diverge().expect("diverge");

        let outcome = rebase_onto_remote(&repo.path, "origin", "main").expect("rebase_onto_remote");

        let RebaseOutcome::Applied { new_sha } = outcome else {
            panic!("expected Applied, got {outcome:?}");
        };
        assert_ne!(
            new_sha, diverged.local_sha,
            "a rebase replaying onto a new base must produce a new commit"
        );
        assert_eq!(head_sha(&repo.path).expect("head_sha"), new_sha);
        let parent = git(&repo.path, &["rev-parse", &format!("{new_sha}^")])
            .expect("rev-parse rebased commit's parent");
        assert_eq!(
            parent, diverged.origin_sha,
            "the rebased commit must sit on top of the fetched remote tip"
        );
        assert!(repo.path.join("local-only.txt").is_file());
        assert!(repo.path.join("origin-only.txt").is_file());
    }

    #[test]
    fn rebase_onto_remote_reports_conflicting_paths_and_leaves_no_rebase_in_progress() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        repo.commit("shared.txt", "base\n")
            .expect("commit shared.txt");
        git(
            &repo.path,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        )
        .expect("push shared.txt to origin");

        std::fs::write(repo.path.join("shared.txt"), "local change\n").expect("write file");
        git(&repo.path, &["add", "shared.txt"]).expect("git add");
        git(&repo.path, &["commit", "--quiet", "-m", "local edit"]).expect("git commit");
        let local_sha = head_sha(&repo.path).expect("head_sha before rebase");

        let shadow = tempfile::tempdir().expect("shadow tempdir");
        git(
            shadow.path(),
            &["clone", "--quiet", &repo.origin.to_string_lossy(), "."],
        )
        .expect("clone shadow");
        std::fs::write(shadow.path().join("shared.txt"), "origin change\n").expect("write file");
        git(shadow.path(), &["add", "shared.txt"]).expect("git add");
        git(shadow.path(), &["commit", "--quiet", "-m", "origin edit"]).expect("git commit");
        git(
            shadow.path(),
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        )
        .expect("push origin edit");

        let outcome = rebase_onto_remote(&repo.path, "origin", "main").expect("rebase_onto_remote");

        let RebaseOutcome::Conflict { paths } = outcome else {
            panic!("expected Conflict, got {outcome:?}");
        };
        assert_eq!(paths, vec![PathBuf::from("shared.txt")]);
        assert!(
            !repo.path.join(".git").join("rebase-merge").exists(),
            "rebase must be aborted, not left in progress"
        );
        assert!(
            !repo.path.join(".git").join("rebase-apply").exists(),
            "rebase must be aborted, not left in progress"
        );
        assert_eq!(
            head_sha(&repo.path).expect("head_sha after abort"),
            local_sha,
            "the worktree must be exactly as it was before the rebase was attempted"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path.join("shared.txt")).expect("read shared.txt"),
            "local change\n",
            "the working tree content must be restored, not left mid-conflict"
        );
    }

    /// Sets up a repo with `modified.txt`, `deleted.txt` and `old_name.txt`
    /// already committed, returns the SHA of that state as `base`, then
    /// commits an add, a modify, a delete and a pure rename on top of it.
    fn repo_with_a_base_and_every_kind_of_change() -> (crate::testing::ScratchRepo, String) {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        repo.commit("modified.txt", "before\n")
            .expect("seed modified.txt");
        repo.commit("deleted.txt", "will be removed\n")
            .expect("seed deleted.txt");
        repo.commit("old_name.txt", "will be renamed\n")
            .expect("seed old_name.txt");
        let base = head_sha(&repo.path).expect("base sha");

        repo.commit("added.txt", "new\n").expect("add file");
        repo.commit("modified.txt", "after\n").expect("modify file");
        git(&repo.path, &["rm", "--quiet", "deleted.txt"]).expect("git rm");
        git(
            &repo.path,
            &["commit", "--quiet", "-m", "delete deleted.txt"],
        )
        .expect("commit delete");
        git(&repo.path, &["mv", "old_name.txt", "new_name.txt"]).expect("git mv");
        git(
            &repo.path,
            &["commit", "--quiet", "-m", "rename old_name.txt"],
        )
        .expect("commit rename");

        (repo, base)
    }

    #[test]
    fn changed_paths_reports_added_modified_deleted_and_renamed_files() {
        let (repo, base) = repo_with_a_base_and_every_kind_of_change();

        let paths = changed_paths(&repo.path, &base).expect("changed_paths");

        assert!(
            paths.contains(&PathBuf::from("added.txt")),
            "added file missing, got {paths:?}"
        );
        assert!(
            paths.contains(&PathBuf::from("modified.txt")),
            "modified file missing, got {paths:?}"
        );
        assert!(
            paths.contains(&PathBuf::from("deleted.txt")),
            "deleted file missing, got {paths:?}"
        );
        assert!(
            paths.contains(&PathBuf::from("new_name.txt")),
            "renamed file's new path missing, got {paths:?}"
        );
        assert!(
            !paths.contains(&PathBuf::from("old_name.txt")),
            "a pure rename must be reported under its new path only, got {paths:?}"
        );
    }

    #[test]
    fn changed_paths_is_empty_when_base_equals_head() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");

        let paths = changed_paths(&repo.path, &repo.seed_sha).expect("changed_paths");

        assert!(paths.is_empty(), "expected no changes, got {paths:?}");
    }

    #[test]
    fn changed_paths_reports_a_binary_file_by_path() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let base = repo.seed_sha.clone();
        std::fs::write(repo.path.join("image.bin"), [0u8, 159, 146, 150, 0, 1, 2])
            .expect("write binary file");
        git(&repo.path, &["add", "image.bin"]).expect("git add");
        git(&repo.path, &["commit", "--quiet", "-m", "add image.bin"]).expect("git commit");

        let paths = changed_paths(&repo.path, &base).expect("changed_paths");

        assert!(
            paths.contains(&PathBuf::from("image.bin")),
            "binary file missing, got {paths:?}"
        );
    }

    #[test]
    fn diff_summary_lists_every_changed_path_and_a_totals_line() {
        let (repo, base) = repo_with_a_base_and_every_kind_of_change();

        let summary = diff_summary(&repo.path, &base).expect("diff_summary");

        assert!(summary.contains("added.txt"), "got {summary:?}");
        assert!(summary.contains("modified.txt"), "got {summary:?}");
        assert!(summary.contains("deleted.txt"), "got {summary:?}");
        assert!(
            summary.contains("old_name.txt") && summary.contains("new_name.txt"),
            "expected the rename to be named on both sides, got {summary:?}"
        );
        assert!(
            summary.contains("file") && summary.contains("changed"),
            "expected a totals line, got {summary:?}"
        );
    }

    #[test]
    fn diff_summary_reports_a_binary_file_without_its_contents() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let base = repo.seed_sha.clone();
        let contents = [0u8, 159, 146, 150, 0, 1, 2];
        std::fs::write(repo.path.join("image.bin"), contents).expect("write binary file");
        git(&repo.path, &["add", "image.bin"]).expect("git add");
        git(&repo.path, &["commit", "--quiet", "-m", "add image.bin"]).expect("git commit");

        let summary = diff_summary(&repo.path, &base).expect("diff_summary");

        assert!(summary.contains("image.bin"), "got {summary:?}");
        assert!(
            summary.contains("Bin"),
            "expected a Bin marker, got {summary:?}"
        );
        assert!(
            !summary
                .as_bytes()
                .windows(contents.len())
                .any(|window| window == contents),
            "summary must not carry the binary content, got {summary:?}"
        );
    }

    #[test]
    fn file_diff_shows_added_and_removed_lines_for_a_modified_file() {
        let (repo, base) = repo_with_a_base_and_every_kind_of_change();

        let diff = file_diff(&repo.path, &base, Path::new("modified.txt")).expect("file_diff");

        assert!(diff.contains("-before"), "got {diff:?}");
        assert!(diff.contains("+after"), "got {diff:?}");
    }

    #[test]
    fn file_diff_reports_deletion_of_the_full_file_content() {
        let (repo, base) = repo_with_a_base_and_every_kind_of_change();

        let diff = file_diff(&repo.path, &base, Path::new("deleted.txt")).expect("file_diff");

        assert!(diff.contains("deleted file mode"), "got {diff:?}");
        assert!(diff.contains("-will be removed"), "got {diff:?}");
    }

    #[test]
    fn file_diff_shows_a_renamed_files_content_under_its_new_path() {
        let (repo, base) = repo_with_a_base_and_every_kind_of_change();

        // A pathspec naming only the new side cannot itself see the paired old
        // side, so git shows the renamed file's content directly rather than
        // a `rename from`/`rename to` header, which needs both paths in
        // scope. This still reports the file's diff without error or content
        // loss, which is what a caller asking for `new_name.txt` needs.
        let diff = file_diff(&repo.path, &base, Path::new("new_name.txt")).expect("file_diff");

        assert!(diff.contains("new_name.txt"), "got {diff:?}");
        assert!(diff.contains("+will be renamed"), "got {diff:?}");
    }

    #[test]
    fn file_diff_reports_a_binary_file_without_its_contents() {
        let repo = crate::testing::scratch_repo().expect("scratch_repo");
        let base = repo.seed_sha.clone();
        let contents = [0u8, 159, 146, 150, 0, 1, 2];
        std::fs::write(repo.path.join("image.bin"), contents).expect("write binary file");
        git(&repo.path, &["add", "image.bin"]).expect("git add");
        git(&repo.path, &["commit", "--quiet", "-m", "add image.bin"]).expect("git commit");

        let diff = file_diff(&repo.path, &base, Path::new("image.bin")).expect("file_diff");

        assert!(
            diff.contains("Binary files") && diff.contains("differ"),
            "got {diff:?}"
        );
        assert!(
            !diff
                .as_bytes()
                .windows(contents.len())
                .any(|window| window == contents),
            "diff must not carry the binary content, got {diff:?}"
        );
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

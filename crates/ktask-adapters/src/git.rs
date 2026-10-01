//! The git command line.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ktask_core::{CommitAllError, Git, GitError, PullRebase, PullRebaseError, PushError};

/// Git, by running the `git` executable found on `PATH`.
#[derive(Debug, Clone, Copy)]
pub struct GitCli;

/// Runs `git` with `args` in `dir`, in the `C` locale so its own words for things are stable.
fn run_git(dir: &Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
}

/// The last line of `output`'s standard error, trimmed — enough of what git said to explain a
/// failure without dumping a whole traceback.
fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

/// `git config --get key`'s value in `dir`, trimmed. `None` when it is unset, blank, or git
/// could not answer.
fn config_value(dir: &Path, key: &str) -> Option<String> {
    let output = run_git(dir, &["config", "--get", key]).ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Whether `dir` has both `user.name` and `user.email` configured.
fn identity_configured(dir: &Path) -> bool {
    config_value(dir, "user.name").is_some() && config_value(dir, "user.email").is_some()
}

/// Runs `git` with `args` in `dir`, named `description` in the error it builds with `fail`
/// when it could not be run at all, or exited non-zero — with its exit code and the tail of
/// its standard error.
fn run_git_checked<E>(
    dir: &Path,
    args: &[&str],
    description: &str,
    fail: impl Fn(String) -> E,
) -> Result<Output, E> {
    let output =
        run_git(dir, args).map_err(|e| fail(format!("{description} could not be run: {e}")))?;
    if !output.status.success() {
        return Err(fail(format!(
            "{description} exited with code {}: {}",
            output.status.code().unwrap_or(-1),
            stderr_of(&output)
        )));
    }
    Ok(output)
}

/// How many commits `remote_ref` has that `HEAD` in `dir` does not, or why it could not be
/// counted.
fn commits_ahead(dir: &Path, remote_ref: &str) -> Result<u64, PullRebaseError> {
    let range = format!("HEAD..{remote_ref}");
    let count_output = run_git_checked(
        dir,
        &["rev-list", "--count", &range],
        &format!("`git rev-list --count {range}`"),
        PullRebaseError::Failed,
    )?;
    Ok(String::from_utf8_lossy(&count_output.stdout)
        .trim()
        .parse()
        .unwrap_or(0))
}

/// Rebases `dir` onto `remote_ref`, `count` commits ahead: [`PullRebase::TookIn`] when it
/// goes cleanly, or every file it conflicted in, with the rebase already undone, when it does
/// not.
fn rebase_onto(dir: &Path, remote_ref: &str, count: u64) -> Result<PullRebase, PullRebaseError> {
    let rebase = run_git(dir, &["rebase", remote_ref]).map_err(|e| {
        PullRebaseError::Failed(format!("`git rebase {remote_ref}` could not be run: {e}"))
    })?;
    if rebase.status.success() {
        return Ok(PullRebase::TookIn(count));
    }
    let conflicted = run_git(dir, &["diff", "--name-only", "--diff-filter=U"])
        .ok()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let _ = run_git(dir, &["rebase", "--abort"]);
    Err(PullRebaseError::Conflict(conflicted))
}

impl Git for GitCli {
    fn work_tree_root(&self, dir: &Path) -> Result<Option<PathBuf>, GitError> {
        let fail = |cause: String| {
            GitError::new(format!(
                "cannot find the git repository of {}: {cause}",
                dir.display()
            ))
        };
        // Git's own words for "not a repository" are what tell it from a real failure, so
        // ask for them in one language.
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(dir)
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| fail(format!("cannot run git: {e}")))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            let root = String::from_utf8(output.stdout)
                .map_err(|_| fail("git printed a path that is not text".to_owned()))?;
            Ok(Some(PathBuf::from(root.trim_end_matches('\n'))))
        } else if stderr.contains("not a git repository") {
            Ok(None)
        } else {
            Err(fail(stderr.trim().to_owned()))
        }
    }

    fn remote_branch_exists(
        &self,
        dir: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<bool, GitError> {
        let output = Command::new("git")
            .args(["ls-remote", "--exit-code", "--heads", remote, branch])
            .current_dir(dir)
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| {
                GitError::new(format!(
                    "cannot check whether {remote}/{branch} exists: cannot run git: {e}"
                ))
            })?;
        Ok(output.status.success())
    }

    fn pull_rebase(
        &self,
        dir: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<PullRebase, PullRebaseError> {
        let status = run_git_checked(
            dir,
            &["status", "--porcelain"],
            "`git status --porcelain`",
            PullRebaseError::Failed,
        )?;
        let dirty = String::from_utf8_lossy(&status.stdout).trim().to_owned();
        if !dirty.is_empty() {
            return Err(PullRebaseError::UncommittedChanges(dirty));
        }

        let fetch = run_git(dir, &["fetch", remote])
            .map_err(|e| PullRebaseError::RemoteUnreachable(format!("cannot run git: {e}")))?;
        if !fetch.status.success() {
            return Err(PullRebaseError::RemoteUnreachable(stderr_of(&fetch)));
        }

        let remote_ref = format!("{remote}/{branch}");
        let count = commits_ahead(dir, &remote_ref)?;
        if count == 0 {
            return Ok(PullRebase::UpToDate);
        }
        rebase_onto(dir, &remote_ref, count)
    }

    fn head(&self, dir: &Path) -> Option<String> {
        let output = run_git(dir, &["rev-parse", "HEAD"]).ok()?;
        if !output.status.success() {
            return None;
        }
        let head = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (!head.is_empty()).then_some(head)
    }

    fn diff_since(&self, dir: &Path, start_commit: &str) -> String {
        match run_git(dir, &["diff", start_commit]) {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
            _ => String::new(),
        }
    }

    fn commit_all(&self, dir: &Path, message: &str) -> Result<Option<String>, CommitAllError> {
        let status = run_git_checked(
            dir,
            &["status", "--porcelain"],
            "`git status --porcelain`",
            CommitAllError::Failed,
        )?;
        if String::from_utf8_lossy(&status.stdout).trim().is_empty() {
            return Ok(None);
        }
        if !identity_configured(dir) {
            return Err(CommitAllError::IdentityNotConfigured);
        }

        run_git_checked(dir, &["add", "-A"], "`git add -A`", CommitAllError::Failed)?;
        run_git_checked(
            dir,
            &["commit", "-m", message],
            "`git commit`",
            CommitAllError::Failed,
        )?;
        let hash = run_git_checked(
            dir,
            &["rev-parse", "--short", "HEAD"],
            "`git rev-parse --short HEAD`",
            CommitAllError::Failed,
        )?;
        Ok(Some(
            String::from_utf8_lossy(&hash.stdout).trim().to_owned(),
        ))
    }

    fn push_and_confirm(
        &self,
        dir: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<String, PushError> {
        let local = run_git_checked(
            dir,
            &["rev-parse", "HEAD"],
            "`git rev-parse HEAD`",
            PushError::Failed,
        )?;
        let local_hash = String::from_utf8_lossy(&local.stdout).trim().to_owned();
        let refspec = format!("HEAD:refs/heads/{branch}");
        let description = format!("`git push {remote} {refspec}`");
        push_ref(dir, remote, &refspec, &description)?;
        confirm_pushed(dir, remote, branch, &local_hash, &description)
    }

    fn reset_tree(&self, dir: &Path, commit: &str) -> Result<(), GitError> {
        run_git_checked(
            dir,
            &["reset", "--hard", commit],
            &format!("`git reset --hard {commit}`"),
            GitError::new,
        )?;
        run_git_checked(dir, &["clean", "-fd"], "`git clean -fd`", GitError::new)?;
        Ok(())
    }
}

/// Pushes `dir`'s `HEAD` to `refspec` on `remote`, named `description` in its error:
/// [`PushError::Rejected`] when git's own words say the branch moved on since, or
/// [`PushError::Failed`] for any other reason it could not be run or exited non-zero.
fn push_ref(dir: &Path, remote: &str, refspec: &str, description: &str) -> Result<(), PushError> {
    let push = run_git(dir, &["push", remote, refspec])
        .map_err(|e| PushError::Failed(format!("{description} could not be run: {e}")))?;
    if push.status.success() {
        return Ok(());
    }
    let tail = stderr_of(&push);
    let rejected = tail.contains("[rejected]") || tail.contains("non-fast-forward");
    Err(if rejected {
        PushError::Rejected
    } else {
        PushError::Failed(format!(
            "{description} exited with code {}: {tail}",
            push.status.code().unwrap_or(-1)
        ))
    })
}

/// Confirms, live against `remote`, that `branch`'s tip there is now `local_hash` — just
/// pushed by `description` — answering with its short form, or why it could not be confirmed.
fn confirm_pushed(
    dir: &Path,
    remote: &str,
    branch: &str,
    local_hash: &str,
    description: &str,
) -> Result<String, PushError> {
    let confirm =
        run_git(dir, &["ls-remote", remote, &format!("refs/heads/{branch}")]).map_err(|e| {
            PushError::Failed(format!(
                "{description} exited zero but confirming it against the remote failed: {e}"
            ))
        })?;
    if !confirm.status.success() {
        return Err(PushError::Failed(format!(
            "{description} exited zero but confirming it against the remote exited with code \
             {}: {}",
            confirm.status.code().unwrap_or(-1),
            stderr_of(&confirm)
        )));
    }
    let tip = String::from_utf8_lossy(&confirm.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned);
    match tip {
        Some(tip) if tip == local_hash => Ok(local_hash[..local_hash.len().min(7)].to_owned()),
        Some(tip) => Err(PushError::Failed(format!(
            "{description} exited zero but {remote}/{branch}'s tip is now {tip}, not \
             {local_hash}: confirm manually before running again"
        ))),
        None => Err(PushError::Failed(format!(
            "{description} exited zero but {remote}/{branch} could not be found afterwards: \
             confirm manually before running again"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn init(dir: &Path) {
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn the_root_of_a_repository_is_found_from_its_subdirectory() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        init(&root);
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(GitCli.work_tree_root(&deep), Ok(Some(root.clone())));
        assert_eq!(GitCli.work_tree_root(&root), Ok(Some(root)));
    }

    #[test]
    fn a_directory_outside_any_repository_has_no_root() {
        let dir = TempDir::new().unwrap();
        assert_eq!(GitCli.work_tree_root(dir.path()), Ok(None));
    }

    #[test]
    fn a_directory_that_does_not_exist_is_an_error_naming_it() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing");
        let error = GitCli.work_tree_root(&missing).unwrap_err().to_string();
        assert!(error.contains(&missing.display().to_string()), "{error}");
    }

    /// A repository at a canonical path, on branch `branch`, with one commit, with `user.name`
    /// and `user.email` configured.
    fn repo_with_branch(branch: &str) -> PathBuf {
        let dir = TempDir::new().unwrap().keep();
        let root = std::fs::canonicalize(&dir).unwrap();
        run(
            &root,
            &["init", "--quiet", &format!("--initial-branch={branch}")],
        );
        configure_identity(&root);
        std::fs::write(root.join("f"), "x").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "--quiet", "-m", "first"]);
        root
    }

    /// A repository cloned from `remote`, at a canonical path, on branch `branch`, tracking
    /// `remote` as `origin`, with `user.name` and `user.email` configured.
    fn clone_of(remote: &Path, branch: &str) -> PathBuf {
        let dir = TempDir::new().unwrap().keep();
        let root = std::fs::canonicalize(&dir).unwrap();
        run(
            dir.parent().unwrap_or(Path::new(".")),
            &[
                "clone",
                "--quiet",
                "--branch",
                branch,
                remote.to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );
        configure_identity(&root);
        root
    }

    fn configure_identity(dir: &Path) {
        run(dir, &["config", "user.email", "t@example.com"]);
        run(dir, &["config", "user.name", "T"]);
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} in {}", dir.display());
    }

    /// Commits `name` with `content` in `dir`, under whatever identity is configured there.
    fn commit_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
        run(dir, &["add", name]);
        run(dir, &["commit", "--quiet", "-m", name]);
    }

    #[test]
    fn an_existing_branch_on_the_remote_is_found() {
        let remote = repo_with_branch("main");
        let cwd = TempDir::new().unwrap();
        assert_eq!(
            GitCli.remote_branch_exists(cwd.path(), remote.to_str().unwrap(), "main"),
            Ok(true)
        );
    }

    #[test]
    fn a_missing_branch_on_the_remote_is_not_found() {
        let remote = repo_with_branch("main");
        let cwd = TempDir::new().unwrap();
        assert_eq!(
            GitCli.remote_branch_exists(cwd.path(), remote.to_str().unwrap(), "other"),
            Ok(false)
        );
    }

    #[test]
    fn an_unknown_remote_is_not_found_rather_than_an_error() {
        let cwd = TempDir::new().unwrap();
        assert_eq!(
            GitCli.remote_branch_exists(cwd.path(), "not-a-remote-at-all", "main"),
            Ok(false)
        );
    }

    #[test]
    fn pull_rebase_brings_in_new_commits_from_the_remote() {
        let remote = repo_with_branch("main");
        let local = clone_of(&remote, "main");
        commit_file(&remote, "new.txt", "from the remote\n");

        let outcome = GitCli.pull_rebase(&local, "origin", "main");

        assert_eq!(outcome, Ok(PullRebase::TookIn(1)));
        assert!(local.join("new.txt").is_file());
    }

    #[test]
    fn pull_rebase_says_up_to_date_when_there_is_nothing_new() {
        let remote = repo_with_branch("main");
        let local = clone_of(&remote, "main");

        assert_eq!(
            GitCli.pull_rebase(&local, "origin", "main"),
            Ok(PullRebase::UpToDate)
        );
    }

    #[test]
    fn pull_rebase_refuses_when_the_directory_has_uncommitted_changes() {
        let remote = repo_with_branch("main");
        let local = clone_of(&remote, "main");
        std::fs::write(local.join("f"), "changed locally\n").unwrap();

        let error = GitCli.pull_rebase(&local, "origin", "main").unwrap_err();

        match error {
            PullRebaseError::UncommittedChanges(status) => {
                assert!(status.contains('f'), "{status}");
            }
            other => panic!("expected UncommittedChanges, got {other:?}"),
        }
    }

    #[test]
    fn pull_rebase_reports_an_unreachable_remote() {
        let local = repo_with_branch("main");
        run(
            &local,
            &["remote", "add", "origin", "/no/such/remote/at/all"],
        );

        let error = GitCli.pull_rebase(&local, "origin", "main").unwrap_err();

        match error {
            PullRebaseError::RemoteUnreachable(_) => {}
            other => panic!("expected RemoteUnreachable, got {other:?}"),
        }
    }

    #[test]
    fn pull_rebase_undoes_a_conflicting_rebase_and_names_the_conflicting_files() {
        let remote = repo_with_branch("main");
        let local = clone_of(&remote, "main");
        commit_file(&local, "f", "local change\n");
        commit_file(&remote, "f", "remote change\n");

        let error = GitCli.pull_rebase(&local, "origin", "main").unwrap_err();

        match error {
            PullRebaseError::Conflict(files) => {
                assert_eq!(files, vec!["f".to_owned()]);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        // The rebase was undone: no rebase left in progress, and the local content is exactly
        // as the local commit left it.
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&local)
            .output()
            .unwrap();
        assert!(status.stdout.is_empty(), "{:?}", status.stdout);
        assert_eq!(
            std::fs::read_to_string(local.join("f")).unwrap(),
            "local change\n"
        );
    }

    #[test]
    fn head_is_the_current_commit() {
        let repo = repo_with_branch("main");
        let head = GitCli.head(&repo).unwrap();
        let expected = run_output(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(head, expected.trim());
    }

    #[test]
    fn head_is_none_for_a_repository_with_no_commits_yet() {
        let dir = TempDir::new().unwrap();
        init(dir.path());
        assert_eq!(GitCli.head(dir.path()), None);
    }

    fn run_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn diff_since_shows_what_changed_since_the_given_commit() {
        let repo = repo_with_branch("main");
        let start = GitCli.head(&repo).unwrap();
        std::fs::write(repo.join("f"), "changed\n").unwrap();

        let diff = GitCli.diff_since(&repo, &start);

        assert!(diff.contains("-x"), "{diff}");
        assert!(diff.contains("+changed"), "{diff}");
    }

    #[test]
    fn diff_since_is_empty_for_an_unknown_commit() {
        let repo = repo_with_branch("main");
        assert_eq!(
            GitCli.diff_since(&repo, "0000000000000000000000000000000000000"),
            ""
        );
    }

    #[test]
    fn reset_tree_discards_a_later_commit_and_uncommitted_and_untracked_changes() {
        let repo = repo_with_branch("main");
        let start = GitCli.head(&repo).unwrap();
        commit_file(&repo, "committed-after.txt", "mess\n");
        std::fs::write(repo.join("f"), "changed\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "new\n").unwrap();

        GitCli.reset_tree(&repo, &start).unwrap();

        assert_eq!(GitCli.head(&repo), Some(start));
        assert_eq!(std::fs::read_to_string(repo.join("f")).unwrap(), "x");
        assert!(!repo.join("committed-after.txt").exists());
        assert!(!repo.join("untracked.txt").exists());
    }

    #[test]
    fn reset_tree_leaves_a_file_already_committed_at_the_target_untouched() {
        let repo = repo_with_branch("main");
        let start = GitCli.head(&repo).unwrap();
        std::fs::write(repo.join("f"), "changed\n").unwrap();

        GitCli.reset_tree(&repo, &start).unwrap();

        assert_eq!(std::fs::read_to_string(repo.join("f")).unwrap(), "x");
    }

    #[test]
    fn commit_all_commits_everything_changed_under_the_configured_identity() {
        let repo = repo_with_branch("main");
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();

        let hash = GitCli.commit_all(&repo, "a message").unwrap().unwrap();

        let logged = run_output(&repo, &["log", "-1", "--format=%h|%an|%ae|%s"]);
        assert!(
            logged.starts_with(&format!("{hash}|T|t@example.com|a message")),
            "{logged}"
        );
        assert!(repo.join("new.txt").is_file());
    }

    #[test]
    fn commit_all_makes_no_commit_when_nothing_changed() {
        let repo = repo_with_branch("main");
        let before = GitCli.head(&repo);

        let outcome = GitCli.commit_all(&repo, "a message").unwrap();

        assert_eq!(outcome, None);
        assert_eq!(GitCli.head(&repo), before);
    }

    #[test]
    fn commit_all_refuses_when_no_identity_is_configured() {
        let dir = TempDir::new().unwrap().keep();
        let repo = std::fs::canonicalize(&dir).unwrap();
        run(&repo, &["init", "--quiet"]);
        // Overrides to empty at the repository level, so the test is hermetic regardless of
        // whatever identity the machine running it has configured globally: local config
        // always wins, and an empty value counts as unset.
        run(&repo, &["config", "user.name", ""]);
        run(&repo, &["config", "user.email", ""]);
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();

        let error = GitCli.commit_all(&repo, "a message").unwrap_err();

        assert_eq!(error, CommitAllError::IdentityNotConfigured);
    }

    #[test]
    fn commit_all_reports_what_git_itself_says_when_it_refuses_the_commit() {
        let repo = repo_with_branch("main");
        let hooks = repo.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'no thanks' >&2\nexit 1\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();

        let error = GitCli.commit_all(&repo, "a message").unwrap_err();

        match error {
            CommitAllError::Failed(message) => {
                assert!(message.contains("git commit"), "{message}");
                assert!(message.contains("exited with code 1"), "{message}");
                assert!(message.contains("no thanks"), "{message}");
            }
            CommitAllError::IdentityNotConfigured => {
                panic!("expected Failed, got IdentityNotConfigured")
            }
        }
    }

    /// A bare repository at a canonical path, seeded with one commit on `branch`, that a
    /// working repository can be cloned from and pushed to as a stand-in for a real remote.
    fn bare_remote(branch: &str) -> PathBuf {
        let dir = TempDir::new().unwrap().keep();
        let bare = std::fs::canonicalize(&dir).unwrap();
        run(
            dir.parent().unwrap_or(Path::new(".")),
            &[
                "init",
                "--quiet",
                "--bare",
                &format!("--initial-branch={branch}"),
                bare.to_str().unwrap(),
            ],
        );
        let seed = repo_with_branch(branch);
        run(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        run(&seed, &["push", "--quiet", "origin", branch]);
        bare
    }

    #[test]
    fn push_and_confirm_pushes_and_the_remotes_tip_matches() {
        let bare = bare_remote("main");
        let local = clone_of(&bare, "main");
        commit_file(&local, "new.txt", "fresh\n");
        let head = GitCli.head(&local).unwrap();

        let hash = GitCli.push_and_confirm(&local, "origin", "main").unwrap();

        assert_eq!(hash, head[..7]);
        let tip = run_output(&bare, &["rev-parse", "refs/heads/main"]);
        assert_eq!(tip.trim(), head);
    }

    #[test]
    fn push_and_confirm_is_rejected_when_the_branch_has_moved_on() {
        let bare = bare_remote("main");
        let local = clone_of(&bare, "main");
        commit_file(&local, "local.txt", "from local\n");

        // Someone else lands a commit on the remote first.
        let other = clone_of(&bare, "main");
        commit_file(&other, "other.txt", "from someone else\n");
        run(&other, &["push", "--quiet", "origin", "main"]);

        let error = GitCli
            .push_and_confirm(&local, "origin", "main")
            .unwrap_err();

        assert_eq!(error, PushError::Rejected);
    }

    #[test]
    fn push_and_confirm_reports_what_git_said_when_the_remote_cannot_be_reached() {
        let local = repo_with_branch("main");
        run(
            &local,
            &["remote", "add", "origin", "/no/such/remote/at/all"],
        );

        let error = GitCli
            .push_and_confirm(&local, "origin", "main")
            .unwrap_err();

        match error {
            PushError::Failed(message) => {
                assert!(message.contains("git push"), "{message}");
            }
            PushError::Rejected => panic!("expected Failed, got Rejected"),
        }
    }
}

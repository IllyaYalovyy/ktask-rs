use crate::storage::{Error, Result};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Command,
};

/// Subjects of the commits ktask makes: a reviewed and validated attempt, and unfinished work
/// committed without review so another task can build on it.
const CHECKPOINT: Subject = Subject("ktask: task #", "");
const PRESERVED: Subject = Subject("ktask: preserve unfinished task #", " for remediation");
const MAX_LISTED_CHANGES: usize = 200;

/// A commit subject: prefix, `N, attempt #M`, suffix.
struct Subject(&'static str, &'static str);
impl Subject {
    fn format(&self, task: i64, attempt: i64) -> String {
        format!("{}{task}, attempt #{attempt}{}", self.0, self.1)
    }
    fn matches(&self, subject: &str) -> bool {
        let numbers = subject
            .strip_prefix(self.0)
            .and_then(|rest| rest.strip_suffix(self.1))
            .and_then(|rest| rest.split_once(", attempt #"));
        numbers.is_some_and(|(task, attempt)| {
            [task, attempt]
                .iter()
                .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
    }
}

/// What an independent review must cover: every change from `base` to the verified `tree`.
pub struct ReviewScope {
    base: String,
    tree: String,
    /// Commits of unfinished work that no review has covered yet, oldest first.
    unreviewed: Vec<String>,
    /// `git diff --name-status` from base to tree, at most MAX_LISTED_CHANGES lines.
    changes: Vec<String>,
    truncated: bool,
}
impl ReviewScope {
    pub fn describe(&self) -> String {
        let (base, tree) = (&self.base, &self.tree);
        let mut text = format!(
            "\nReview scope, computed by the supervisor: `git diff {base} {tree}` is the complete change to review, including new files.\n"
        );
        if !self.unreviewed.is_empty() {
            text.push_str(&format!(
                "It includes unfinished work that was committed without review so another task could build on it:\n{}\nReview that work as part of this change. It may be incomplete by design while its own task is still open: judge its quality, not its completeness.\n",
                self.unreviewed.join("\n")
            ));
        }
        if self.changes.is_empty() {
            text.push_str("Changed files: none; nothing changed since the review base.\n");
        } else {
            text.push_str(&format!("Changed files:\n{}\n", self.changes.join("\n")));
            if self.truncated {
                text.push_str("(list truncated; the diff above is complete)\n");
            }
        }
        text
    }
}

pub struct Workspace {
    pub root: PathBuf,
    pub worker_lock: PathBuf,
    _lease: File,
}
pub const GIT_IDENTITY_FIX: &str =
    "git config user.name 'Your Name' && git config user.email 'you@example.com'";

/// Resolve Git's effective project configuration, including local and included config.
/// Do not allow Git's fallback hostname identity to author supervisor commits.
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

pub fn git_identity(root: &Path) -> Result<GitIdentity> {
    let name = git(root, &["config", "--get", "user.name"]);
    let email = git(root, &["config", "--get", "user.email"]);
    match (name, email) {
        (Ok(name), Ok(email)) if !name.is_empty() && !email.is_empty() => {
            Ok(GitIdentity { name, email })
        }
        _ => Err(Error::Conflict(format!(
            "Git commit identity is not configured for this project; run {GIT_IDENTITY_FIX}"
        ))),
    }
}
pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(Error::Conflict(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(std::io::Error::other)?
        .trim()
        .into())
}
impl Workspace {
    pub fn supervisor_alive(root: &Path) -> Result<bool> {
        let common = match git(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ) {
            Ok(path) => PathBuf::from(path),
            Err(Error::Conflict(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        let path = common.join("ktask-runner.lock");
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(super::process::inspect(&file, |_| Ok(()))?.is_none())
    }
    pub fn acquire(root: &Path) -> Result<Self> {
        Self::acquire_with_cleanup_confirmation(root, false)
    }
    pub fn acquire_with_cleanup_confirmation(root: &Path, confirmed: bool) -> Result<Self> {
        let root = root.canonicalize()?;
        let top = PathBuf::from(git(&root, &["rev-parse", "--show-toplevel"])?).canonicalize()?;
        if root != top {
            return Err(Error::Conflict(
                "Project must be the Git repository root".into(),
            ));
        }
        let common = PathBuf::from(git(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        let lease = super::process::lock(&common.join("ktask-runner.lock"))?;
        let worker_lock = common.join("ktask-worker.lock");
        // A guardian survives its supervisor long enough to stop/reap workers. Never overlap it.
        let worker = super::process::lock(&worker_lock)?;
        if worker.metadata()?.len() != 0 {
            if !confirmed {
                return Err(Error::Conflict(format!(
                    "Worker died without cleanup confirmation. Execution is blocked. Inspect/stop its descendants and preserve work, then use recover --confirm-worker-cleanup. Marker: {}",
                    worker_lock.display()
                )));
            }
            worker.set_len(0)?;
            worker.sync_all()?;
        }
        drop(worker);
        git(&root,&["rev-parse","--verify","HEAD"]).map_err(|_|Error::Conflict("Repository needs an initial baseline commit before execution; no files were changed".into()))?;
        Ok(Self {
            root,
            worker_lock,
            _lease: lease,
        })
    }
    pub fn head(&self) -> Result<String> {
        git(&self.root, &["rev-parse", "HEAD"])
    }
    pub fn clean(&self) -> Result<bool> {
        Ok(git(&self.root, &["status", "--porcelain"])?.is_empty())
    }
    /// Canonical tree identity of the current bytes and modes, independent of
    /// whether a change is staged. The caller holds the workspace lease.
    pub fn reset_fingerprint(&self) -> Result<String> {
        // Scratch sweeps its root for abandoned run directories. Give it an
        // owned private root so a normal launch with TMPDIR=/tmp cannot sweep
        // another application's directories.
        let root = tempfile::Builder::new().prefix("ktask-reset-").tempdir()?;
        self.fingerprint(root.path())
    }
    /// Hash the exact tracked/untracked content a checkpoint would include, without
    /// changing the user's index. Git's tree identity includes paths, modes and bytes.
    pub fn fingerprint(&self, scratch_root: &Path) -> Result<String> {
        let scratch = crate::scratch::Scratch::new(scratch_root)?;
        let index = git(
            &self.root,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        let index = Path::new(&index);
        if index.exists() {
            std::fs::copy(index, scratch.path().join("index"))?;
        }
        let mut tree = String::new();
        for args in [&["add", "--all"][..], &["write-tree"][..]] {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.fsmonitor=false",
                ])
                .args(args)
                .env("GIT_INDEX_FILE", scratch.path().join("index"))
                .env("TMPDIR", scratch.path())
                .output()?;
            if !output.status.success() {
                return Err(Error::Conflict(format!(
                    "Cannot fingerprint worktree: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            tree = String::from_utf8(output.stdout)
                .map_err(std::io::Error::other)?
                .trim()
                .into();
        }
        scratch.close()?;
        Ok(tree)
    }
    /// The review of an attempt that started at `attempt_base` covers its own work and any
    /// unfinished work committed without review since the last checkpoint, which every
    /// successful review covers in full.
    pub fn review_scope(&self, attempt_base: &str, tree: &str) -> Result<ReviewScope> {
        let history = git(
            &self.root,
            &[
                "log",
                "--first-parent",
                "--fixed-strings",
                "--grep=ktask: ",
                "--format=%H %P%x09%s",
                attempt_base,
            ],
        )?;
        let mut base = attempt_base.to_owned();
        let mut unreviewed = Vec::new();
        for line in history.lines() {
            let (ids, subject) = line.split_once('\t').unwrap_or((line, ""));
            if CHECKPOINT.matches(subject) {
                break;
            }
            let mut ids = ids.split(' ');
            if let (Some(commit), Some(parent)) = (ids.next(), ids.next())
                && PRESERVED.matches(subject)
            {
                base = parent.to_owned();
                unreviewed.insert(0, format!("{commit} {subject}"));
            }
        }
        let mut changes: Vec<String> = git(&self.root, &["diff", "--name-status", &base, tree])?
            .lines()
            .map(String::from)
            .collect();
        let truncated = changes.len() > MAX_LISTED_CHANGES;
        changes.truncate(MAX_LISTED_CHANGES);
        Ok(ReviewScope {
            base,
            tree: tree.to_owned(),
            unreviewed,
            changes,
            truncated,
        })
    }
    pub fn checkpoint(
        &self,
        task: i64,
        attempt: i64,
        expected_head: &str,
        verified_tree: &str,
        allow_amend: bool,
    ) -> Result<String> {
        if self.head()? != expected_head {
            return Err(Error::Conflict(
                "HEAD changed after checkpoint intent; work retained".into(),
            ));
        }
        if allow_amend
            && git(&self.root, &["log", "-1", "--format=%s"])? == CHECKPOINT.format(task, attempt)
        {
            if self.clean()? {
                if git(&self.root, &["rev-parse", "HEAD^{tree}"])? == verified_tree {
                    return self.head();
                }
            } else {
                git(&self.root, &["add", "--all"])?;
                if git(&self.root, &["write-tree"])? != verified_tree {
                    return Err(Error::Conflict(
                        "Worktree changed after validation; verification must run again".into(),
                    ));
                }
                self.git_commit(&["commit", "--amend", "--no-edit", "--reset-author"])?;
                return self.head();
            }
        }
        self.commit(
            expected_head,
            &CHECKPOINT.format(task, attempt),
            Some(verified_tree),
        )
    }
    pub fn preserve(&self, task: i64, attempt: i64, base: &str) -> Result<String> {
        self.commit(base, &PRESERVED.format(task, attempt), None)
    }
    fn commit(&self, base: &str, message: &str, verified_tree: Option<&str>) -> Result<String> {
        if self.head()? != base {
            return Err(Error::Conflict(
                "Agent changed HEAD; preserve work and inspect before continuing".into(),
            ));
        }
        if !self.clean()? {
            git(&self.root, &["add", "--all"])?;
            if let Some(expected) = verified_tree
                && git(&self.root, &["write-tree"])? != expected
            {
                return Err(Error::Conflict(
                    "Worktree changed after validation; work retained, verification must run again"
                        .into(),
                ));
            }
            self.git_commit(&["commit", "-m", message])?;
        } else if let Some(expected) = verified_tree
            && git(&self.root, &["rev-parse", "HEAD^{tree}"])? != expected
        {
            return Err(Error::Conflict(
                "Worktree changed after validation; verification must run again".into(),
            ));
        }
        self.head()
    }
    fn git_commit(&self, args: &[&str]) -> Result<()> {
        let identity = git_identity(&self.root)?;
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_AUTHOR_NAME", &identity.name)
            .env("GIT_AUTHOR_EMAIL", &identity.email)
            .env("GIT_COMMITTER_NAME", &identity.name)
            .env("GIT_COMMITTER_EMAIL", &identity.email)
            .output()?;
        if !output.status.success() {
            return Err(Error::Conflict(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod reset_fingerprint_tests {
    use super::*;

    fn run(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn staging_same_bytes_keeps_reset_identity_and_new_edit_changes_it() -> Result<()> {
        let project = tempfile::Builder::new()
            .prefix("ktask-reset-test-")
            .tempdir()?;
        let root = project.path();
        run(root, &["init", "-q"]);
        std::fs::write(root.join("tracked.txt"), "baseline\n")?;
        run(root, &["add", "tracked.txt"]);
        run(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "baseline",
            ],
        );
        run(root, &["config", "user.name", "Test"]);
        run(root, &["config", "user.email", "test@example.com"]);
        let workspace = Workspace::acquire(root)?;
        std::fs::write(root.join("tracked.txt"), "reset bytes\n")?;
        std::fs::write(root.join("new.txt"), "new bytes\n")?;
        let saved = workspace.reset_fingerprint()?;
        let index_before = std::fs::read(root.join(".git/index"))?;
        run(root, &["add", "tracked.txt", "new.txt"]);
        let staged_index = std::fs::read(root.join(".git/index"))?;
        assert_ne!(index_before, staged_index);
        assert_eq!(workspace.reset_fingerprint()?, saved);
        assert_eq!(std::fs::read(root.join(".git/index"))?, staged_index);
        std::fs::write(root.join("new.txt"), "newer bytes\n")?;
        assert_ne!(workspace.reset_fingerprint()?, saved);
        Ok(())
    }

    #[test]
    fn amended_checkpoint_uses_current_project_author_and_committer() -> Result<()> {
        let private = tempfile::Builder::new()
            .prefix("ktask-identity-test-")
            .tempdir()?;
        let root = private.path().join("project");
        std::fs::create_dir(&root)?;
        run(&root, &["init", "-q"]);
        run(&root, &["config", "user.name", "First Author"]);
        run(&root, &["config", "user.email", "first@example.com"]);
        std::fs::write(root.join("tracked.txt"), "baseline\n")?;
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-qm", "baseline"]);
        let workspace = Workspace::acquire(&root)?;
        let base = workspace.head()?;
        std::fs::write(root.join("tracked.txt"), "first version\n")?;
        let tree = workspace.fingerprint(private.path())?;
        let first = workspace.checkpoint(1, 1, &base, &tree, false)?;
        assert_eq!(
            git(&root, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"])?,
            "First Author <first@example.com>|First Author <first@example.com>"
        );
        run(&root, &["config", "user.name", "Current Author"]);
        run(&root, &["config", "user.email", "current@example.com"]);
        std::fs::write(root.join("tracked.txt"), "verified amendment\n")?;
        let tree = workspace.fingerprint(private.path())?;
        let amended = workspace.checkpoint(1, 1, &first, &tree, true)?;
        assert_ne!(amended, first);
        assert_eq!(
            git(&root, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"])?,
            "Current Author <current@example.com>|Current Author <current@example.com>"
        );
        Ok(())
    }
}

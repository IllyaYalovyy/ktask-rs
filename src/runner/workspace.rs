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
        base: &str,
        verified_tree: &str,
    ) -> Result<String> {
        self.commit(base, &CHECKPOINT.format(task, attempt), Some(verified_tree))
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
            git(
                &self.root,
                &[
                    "-c",
                    "user.name=ktask",
                    "-c",
                    "user.email=ktask@localhost",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "commit",
                    "-m",
                    message,
                ],
            )?;
        } else if let Some(expected) = verified_tree
            && git(&self.root, &["rev-parse", "HEAD^{tree}"])? != expected
        {
            return Err(Error::Conflict(
                "Worktree changed after validation; verification must run again".into(),
            ));
        }
        self.head()
    }
}

use crate::storage::{Error, Result};
use std::{
    collections::HashSet,
    fs::File,
    io::Write,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
    _lease: super::process::Lease,
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
pub fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Error::Conflict("Cannot establish Git ancestry".into())),
    }
}
/// Uncommitted work belongs to the task whose attempt started at `base`, also when the
/// owner committed on top of that base while the task was paused or failed.
pub fn retains_work_from(root: &Path, base: Option<&str>, head: &str) -> Result<bool> {
    match base {
        Some(base) => Ok(base == head || is_ancestor(root, base, head)?),
        None => Ok(false),
    }
}
pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .env("LC_ALL", "C")
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
            // A guardian died without cleaning up: stop its workers, then clear its marker.
            let mut marker = Vec::new();
            std::io::Read::read_to_end(&mut &*worker, &mut marker)?;
            super::process::stop_orphaned_workers(&marker)?;
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
        Self::fingerprint_at(&self.root, scratch_root)
    }
    /// The guardian holds the worker lease while archiving, so it must fingerprint
    /// without trying to acquire the supervisor's separate runner lease.
    pub(crate) fn fingerprint_at(root: &Path, scratch_root: &Path) -> Result<String> {
        let scratch = crate::scratch::Scratch::new(scratch_root)?;
        let index = git(
            root,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        let index = Path::new(&index);
        if index.exists() {
            std::fs::copy(index, scratch.path().join("index"))?;
        }
        let tree = Self::stage_tree_at(
            root,
            Some(&scratch.path().join("index")),
            Some(scratch.path()),
        )?;
        scratch.close()?;
        Ok(tree)
    }
    fn git_for_index_at(
        root: &Path,
        index: Option<&Path>,
        scratch: Option<&Path>,
        args: &[&str],
    ) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        if let Some(scratch) = scratch {
            command.env("TMPDIR", scratch);
        }
        command
    }
    fn git_output(mut command: Command, input: Option<&[u8]>, action: &str) -> Result<Vec<u8>> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn()?;
        let written = input.map_or(Ok(()), |input| child.stdin.take().unwrap().write_all(input));
        let output = child.wait_with_output()?;
        written?;
        if !output.status.success() {
            return Err(Error::Conflict(format!(
                "Cannot {action}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }
    /// Stage current bytes using Git's ordinary clean-filter behavior. Git can
    /// trust stale index stat data after a rapid same-size edit; raw blob hashes
    /// identify only those entries that need their content staged explicitly.
    fn stage_tree(&self, index: Option<&Path>, scratch: Option<&Path>) -> Result<String> {
        Self::stage_tree_at(&self.root, index, scratch)
    }
    fn stage_tree_at(root: &Path, index: Option<&Path>, scratch: Option<&Path>) -> Result<String> {
        Self::git_output(
            Self::git_for_index_at(root, index, scratch, &["add", "--all"]),
            None,
            "stage worktree",
        )?;
        let listing = Self::git_output(
            Self::git_for_index_at(
                root,
                index,
                scratch,
                &["ls-files", "--stage", "-z", "--sparse"],
            ),
            None,
            "list staged files",
        )?;
        let flags = Self::git_output(
            Self::git_for_index_at(root, index, scratch, &["ls-files", "-t", "-z", "--sparse"]),
            None,
            "read staged file flags",
        )?;
        let excluded: HashSet<&[u8]> = flags
            .split(|byte| *byte == 0)
            .filter_map(|record| record.strip_prefix(b"S "))
            .collect();
        for record in listing
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| Error::Conflict("Invalid staged file record".into()))?;
            let header = std::str::from_utf8(&record[..tab]).map_err(std::io::Error::other)?;
            let mut fields = header.split_whitespace();
            let (Some(mode), Some(staged_oid), Some("0")) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if mode == "160000" || mode == "040000" {
                continue;
            }
            let path = PathBuf::from(std::ffi::OsString::from_vec(record[tab + 1..].to_vec()));
            let file = root.join(&path);
            let metadata = match std::fs::symlink_metadata(&file) {
                Ok(metadata) => metadata,
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && excluded.contains(path.as_os_str().as_bytes()) =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let link = metadata.file_type().is_symlink();
            let link_target = if link {
                Some(std::fs::read_link(&file)?)
            } else {
                None
            };
            let raw = if let Some(target) = &link_target {
                Self::git_output(
                    Self::git_for_index_at(
                        root,
                        index,
                        scratch,
                        &["hash-object", "--no-filters", "--stdin"],
                    ),
                    Some(target.as_os_str().as_bytes()),
                    "hash symlink",
                )?
            } else {
                let mut command = Self::git_for_index_at(
                    root,
                    index,
                    scratch,
                    &["hash-object", "--no-filters", "--"],
                );
                command.arg(&path);
                Self::git_output(command, None, "hash worktree file")?
            };
            if raw.strip_suffix(b"\n").unwrap_or(&raw) == staged_oid.as_bytes() {
                continue;
            }
            let converted = if let Some(target) = &link_target {
                Self::git_output(
                    Self::git_for_index_at(
                        root,
                        index,
                        scratch,
                        &["hash-object", "-w", "--no-filters", "--stdin"],
                    ),
                    Some(target.as_os_str().as_bytes()),
                    "stage symlink target",
                )?
            } else {
                let mut command =
                    Self::git_for_index_at(root, index, scratch, &["hash-object", "-w", "--path"]);
                command.arg(&path).arg("--").arg(&path);
                Self::git_output(command, None, "stage changed file")?
            };
            let converted =
                std::str::from_utf8(converted.strip_suffix(b"\n").unwrap_or(&converted))
                    .map_err(std::io::Error::other)?;
            if converted != staged_oid {
                let mut entry = format!("{mode} {converted}\t").into_bytes();
                entry.extend_from_slice(path.as_os_str().as_bytes());
                entry.push(0);
                Self::git_output(
                    Self::git_for_index_at(
                        root,
                        index,
                        scratch,
                        &["update-index", "-z", "--index-info"],
                    ),
                    Some(&entry),
                    "update changed file",
                )?;
            }
        }
        let tree = Self::git_output(
            Self::git_for_index_at(root, index, scratch, &["write-tree"]),
            None,
            "write staged tree",
        )?;
        Ok(String::from_utf8(tree)
            .map_err(std::io::Error::other)?
            .trim()
            .to_owned())
    }
    pub fn tree_difference(&self, expected: &str, observed: &str) -> Result<String> {
        let paths = git(
            &self.root,
            &["diff", "--name-status", "--no-renames", expected, observed],
        )?;
        if paths.is_empty() {
            Ok("none (tree identities differ without a path diff)".into())
        } else {
            Ok(paths)
        }
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
            let staged = self.stage_tree(None, None)?;
            if staged != verified_tree {
                return Err(Error::Conflict(
                    "Worktree changed after validation; verification must run again".into(),
                ));
            }
            if staged == git(&self.root, &["rev-parse", "HEAD^{tree}"])? {
                return self.head();
            }
            self.git_commit(&["commit", "--amend", "--no-edit", "--reset-author"])?;
            return self.head();
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
        let staged = self.stage_tree(None, None)?;
        if let Some(expected) = verified_tree
            && staged != expected
        {
            return Err(Error::Conflict(
                "Worktree changed after validation; work retained, verification must run again"
                    .into(),
            ));
        }
        if staged != git(&self.root, &["rev-parse", "HEAD^{tree}"])? {
            self.git_commit(&["commit", "-m", message])?;
        }
        self.head()
    }
    fn git_commit(&self, args: &[&str]) -> Result<()> {
        let identity = git_identity(&self.root)?;
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["-c", "commit.gpgsign=false"])
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
mod fingerprint_tests {
    use super::*;

    fn fingerprint(workspace: &Workspace) -> Result<String> {
        let scratch_root = tempfile::tempdir()?;
        workspace.fingerprint(scratch_root.path())
    }

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
        let saved = fingerprint(&workspace)?;
        let index_before = std::fs::read(root.join(".git/index"))?;
        run(root, &["add", "tracked.txt", "new.txt"]);
        let staged_index = std::fs::read(root.join(".git/index"))?;
        assert_ne!(index_before, staged_index);
        assert_eq!(fingerprint(&workspace)?, saved);
        assert_eq!(std::fs::read(root.join(".git/index"))?, staged_index);
        std::fs::write(root.join("new.txt"), "newer bytes\n")?;
        assert_ne!(fingerprint(&workspace)?, saved);
        Ok(())
    }

    #[test]
    fn same_size_edit_with_preserved_mtime_changes_fingerprint_without_changing_real_index()
    -> Result<()> {
        let project = tempfile::Builder::new()
            .prefix("ktask-fingerprint-test-")
            .tempdir()?;
        let root = project.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "core.trustctime", "false"]);
        let file = root.join("result.txt");
        std::fs::write(&file, "verified 1\n")?;
        let original_mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800);
        std::fs::File::options()
            .write(true)
            .open(&file)?
            .set_times(std::fs::FileTimes::new().set_modified(original_mtime))?;
        run(root, &["add", "result.txt"]);
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
        let baseline = git(root, &["rev-parse", "HEAD^{tree}"])?;
        let head = git(root, &["rev-parse", "HEAD"])?;
        run(root, &["config", "user.name", "Test"]);
        run(root, &["config", "user.email", "test@example.com"]);
        let real_index = std::fs::read(root.join(".git/index"))?;
        std::fs::write(&file, "verified 2\n")?;
        std::fs::File::options()
            .write(true)
            .open(&file)?
            .set_times(std::fs::FileTimes::new().set_modified(original_mtime))?;
        let workspace = Workspace::acquire(root)?;
        let first = fingerprint(&workspace)?;
        assert_ne!(first, baseline, "same-size edit must be hashed");
        assert_eq!(fingerprint(&workspace)?, first);
        assert_eq!(std::fs::read(root.join(".git/index"))?, real_index);
        let committed = workspace.checkpoint(1, 1, &head, &first, false)?;
        assert_eq!(workspace.head()?, committed);
        assert_eq!(git(root, &["rev-parse", "HEAD^{tree}"])?, first);
        assert_eq!(git(root, &["show", "HEAD:result.txt"])?, "verified 2");
        Ok(())
    }

    #[test]
    fn attributes_change_keeps_ordinary_crlf_blobs_through_checkpoint_and_amendment() -> Result<()>
    {
        let project = tempfile::Builder::new()
            .prefix("ktask-attributes-test-")
            .tempdir()?;
        let repo = project.path().join("repo");
        std::fs::create_dir(&repo)?;
        let root = repo.as_path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "Test"]);
        run(root, &["config", "user.email", "test@example.com"]);
        run(root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("source.txt"), b"original\r\n")?;
        run(root, &["add", "source.txt"]);
        run(root, &["commit", "-qm", "baseline"]);
        let workspace = Workspace::acquire(root)?;
        let base = workspace.head()?;

        std::fs::write(root.join(".gitattributes"), "*.txt text=auto\n")?;
        let index_before = std::fs::read(root.join(".git/index"))?;
        let tree = workspace.fingerprint(project.path())?;
        assert_eq!(std::fs::read(root.join(".git/index"))?, index_before);
        let committed = workspace.checkpoint(1, 1, &base, &tree, false)?;
        assert_eq!(git(root, &["rev-parse", "HEAD^{tree}"])?, tree);
        let committed_blob = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["show", "HEAD:source.txt"])
            .output()?;
        assert!(committed_blob.status.success());
        assert_eq!(committed_blob.stdout, b"original\r\n");
        assert_eq!(std::fs::read(root.join("source.txt"))?, b"original\r\n");

        std::fs::write(
            root.join(".gitattributes"),
            "*.txt text=auto\n# unchanged source\n",
        )?;
        let index_before = std::fs::read(root.join(".git/index"))?;
        let amended_tree = workspace.fingerprint(project.path())?;
        assert_eq!(std::fs::read(root.join(".git/index"))?, index_before);
        let amended = workspace.checkpoint(1, 1, &committed, &amended_tree, true)?;
        assert_ne!(amended, committed);
        assert_eq!(git(root, &["rev-parse", "HEAD^{tree}"])?, amended_tree);
        let amended_blob = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["show", "HEAD:source.txt"])
            .output()?;
        assert!(amended_blob.status.success());
        assert_eq!(amended_blob.stdout, b"original\r\n");

        assert_eq!(
            workspace.checkpoint(1, 1, &amended, &amended_tree, true)?,
            amended
        );
        assert_eq!(
            workspace.checkpoint(2, 1, &amended, &amended_tree, false)?,
            amended
        );
        std::fs::write(root.join("source.txt"), b"changed!\r\n")?;
        assert!(
            workspace
                .checkpoint(1, 1, &amended, &amended_tree, true)
                .unwrap_err()
                .to_string()
                .contains("Worktree changed after validation")
        );
        Ok(())
    }

    fn sparse_project(sparse_index: bool) -> Result<(tempfile::TempDir, PathBuf)> {
        let project = tempfile::Builder::new()
            .prefix("ktask-sparse-test-")
            .tempdir()?;
        let repo = project.path().join("repo");
        std::fs::create_dir(&repo)?;
        run(&repo, &["init", "-q"]);
        run(&repo, &["config", "user.name", "Test"]);
        run(&repo, &["config", "user.email", "test@example.com"]);
        std::fs::create_dir(repo.join("kept"))?;
        std::fs::create_dir(repo.join("excluded"))?;
        std::fs::write(repo.join("kept/visible.txt"), "first\n")?;
        std::fs::write(repo.join("excluded/hidden.txt"), "preserved\n")?;
        std::fs::write(repo.join("excluded/line\nbreak.txt"), "also preserved\n")?;
        run(&repo, &["add", "--all"]);
        run(&repo, &["commit", "-qm", "baseline"]);
        run(
            &repo,
            &[
                "sparse-checkout",
                "set",
                "--cone",
                if sparse_index {
                    "--sparse-index"
                } else {
                    "--no-sparse-index"
                },
                "kept",
            ],
        );
        let entries = git(&repo, &["ls-files", "--stage", "--sparse"])?;
        assert!(
            entries.contains(if sparse_index {
                "excluded/"
            } else {
                "excluded/hidden.txt"
            }),
            "{entries}"
        );
        Ok((project, repo))
    }

    fn assert_excluded_blobs(root: &Path) -> Result<()> {
        assert_eq!(
            git(root, &["show", "HEAD:excluded/hidden.txt"])?,
            "preserved"
        );
        assert_eq!(
            git(root, &["show", "HEAD:excluded/line\nbreak.txt"])?,
            "also preserved"
        );
        assert!(!root.join("excluded/hidden.txt").exists());
        assert!(!root.join("excluded/line\nbreak.txt").exists());
        Ok(())
    }

    #[test]
    fn sparse_checkout_preserves_absent_blobs_with_ordinary_and_sparse_indexes() -> Result<()> {
        for sparse_index in [false, true] {
            let (project, repo) = sparse_project(sparse_index)?;
            let root = repo.as_path();
            assert_excluded_blobs(root)?;
            let workspace = Workspace::acquire(root)?;
            let base = workspace.head()?;
            let baseline = git(root, &["rev-parse", "HEAD^{tree}"])?;
            let real_index = std::fs::read(root.join(".git/index"))?;
            assert_eq!(workspace.fingerprint(project.path())?, baseline);
            assert_eq!(workspace.fingerprint(project.path())?, baseline);
            assert_eq!(std::fs::read(root.join(".git/index"))?, real_index);
            assert_excluded_blobs(root)?;

            std::fs::write(root.join("kept/visible.txt"), "second\n")?;
            let tree = workspace.fingerprint(project.path())?;
            assert_ne!(tree, baseline);
            assert_eq!(workspace.fingerprint(project.path())?, tree);
            assert_eq!(std::fs::read(root.join(".git/index"))?, real_index);
            let committed = workspace.checkpoint(1, 1, &base, &tree, false)?;
            assert_eq!(git(root, &["rev-parse", "HEAD^{tree}"])?, tree);
            assert_excluded_blobs(root)?;

            std::fs::write(root.join("kept/visible.txt"), "third\n")?;
            let amended_tree = workspace.fingerprint(project.path())?;
            assert_ne!(amended_tree, tree);
            let amended = workspace.checkpoint(1, 1, &committed, &amended_tree, true)?;
            assert_ne!(amended, committed);
            assert_eq!(git(root, &["rev-parse", "HEAD^{tree}"])?, amended_tree);
            assert_excluded_blobs(root)?;
        }
        Ok(())
    }

    #[test]
    fn checkpoint_and_amend_respect_commit_hooks() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let project = tempfile::tempdir()?;
        let root = project.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "Illya Yalovyy"]);
        run(root, &["config", "user.email", "yalovoy@gmail.com"]);
        std::fs::write(root.join("tracked.txt"), "baseline\n")?;
        run(root, &["add", "tracked.txt"]);
        run(root, &["commit", "-qm", "Create baseline"]);
        let hooks = root.join(".git/test-hooks");
        std::fs::create_dir(&hooks)?;
        run(root, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
        let hook = hooks.join("commit-msg");
        std::fs::write(&hook, "#!/bin/sh\necho hook-rejected >&2\nexit 1\n")?;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        let workspace = Workspace::acquire(root)?;
        let base = workspace.head()?;
        std::fs::write(root.join("tracked.txt"), "first\n")?;
        let tree = fingerprint(&workspace)?;
        let error = workspace.checkpoint(1, 1, &base, &tree, false).unwrap_err();
        assert!(error.to_string().contains("hook-rejected"));
        assert_eq!(workspace.head()?, base);
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n")?;
        let committed = workspace.checkpoint(1, 1, &base, &tree, false)?;
        std::fs::write(&hook, "#!/bin/sh\necho hook-rejected >&2\nexit 1\n")?;
        std::fs::write(root.join("tracked.txt"), "second\n")?;
        let tree = fingerprint(&workspace)?;
        let error = workspace
            .checkpoint(1, 1, &committed, &tree, true)
            .unwrap_err();
        assert!(error.to_string().contains("hook-rejected"));
        assert_eq!(workspace.head()?, committed);
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

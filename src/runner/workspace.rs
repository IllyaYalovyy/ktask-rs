use crate::storage::{Error, Result};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Command,
};

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
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
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
    pub fn checkpoint(&self, task: i64, attempt: i64, base: &str) -> Result<String> {
        if self.head()? != base {
            return Err(Error::Conflict(
                "Agent changed HEAD; preserve work and inspect before continuing".into(),
            ));
        }
        if !self.clean()? {
            git(&self.root, &["add", "--all"])?;
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
                    &format!("ktask: task #{task}, attempt #{attempt}"),
                ],
            )?;
        }
        self.head()
    }
}

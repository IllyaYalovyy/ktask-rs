//! Read-only turns run in a bubblewrap mount namespace in which the project, ktask's state and
//! configuration, and the supervisor executable are mounted read-only. That holds whatever
//! tools, settings or hooks a provider allows; a provider's own sandbox still applies inside.
use crate::{
    instance::Paths,
    storage::{Error, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

const UNAVAILABLE: &str =
    "Read-only review needs bubblewrap (bwrap) with unprivileged user namespaces";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sandbox {
    bwrap: PathBuf,
    read_only: Vec<PathBuf>,
}

impl Sandbox {
    pub fn for_project(project: &Path, paths: &Paths) -> Result<Self> {
        // A protected directory must exist, or the turn could create it.
        for directory in [&paths.state, &paths.config] {
            std::fs::create_dir_all(directory)?;
        }
        let read_only = [project, &paths.state, &paths.config, &paths.executable]
            .into_iter()
            .map(Path::canonicalize)
            .collect::<std::io::Result<_>>()?;
        Ok(Self {
            bwrap: bwrap()?,
            read_only,
        })
    }

    /// A command that runs `program` inside the sandbox. The turn's own scratch directory
    /// stays writable even when it lies inside a protected path.
    pub fn command(&self, program: &Path, scratch: &Path) -> Command {
        let mut command = Command::new(&self.bwrap);
        command.args(["--dev-bind", "/", "/"]);
        for path in &self.read_only {
            command.arg("--ro-bind").arg(path).arg(path);
        }
        command.arg("--bind").arg(scratch).arg(scratch);
        command.args(["--die-with-parent", "--"]).arg(program);
        command
    }
}

/// Fails with the reason when bubblewrap is missing or cannot create a namespace here.
pub fn available() -> Result<()> {
    let output = Command::new(bwrap()?)
        .args(["--dev-bind", "/", "/", "--", "true"])
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Conflict(format!(
        "{UNAVAILABLE}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn bwrap() -> Result<PathBuf> {
    super::executable("bwrap")
        .map_err(|_| Error::Conflict(format!("{UNAVAILABLE}; install bubblewrap")))
}

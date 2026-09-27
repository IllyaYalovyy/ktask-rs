//! Runs the real `ktask-rs` binary in a sandbox of its own.
//!
//! The child gets a temporary `HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `TMPDIR` and
//! nothing else from the test process's environment except `PATH`. The test process itself
//! never changes its environment or working directory: everything is set on the child.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

pub(crate) type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// What a finished run left behind.
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    /// `None` when the process was killed by a signal.
    pub(crate) code: Option<i32>,
}

/// A throwaway home, config, state and temp directory, deleted on drop.
#[derive(Debug)]
pub(crate) struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    pub(crate) fn new() -> Result<Self> {
        let sandbox = Self {
            root: TempDir::new()?,
        };
        for dir in [
            sandbox.home(),
            sandbox.config_home(),
            sandbox.state_home(),
            sandbox.tmpdir(),
        ] {
            std::fs::create_dir(dir)?;
        }
        Ok(sandbox)
    }

    pub(crate) fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    pub(crate) fn config_home(&self) -> PathBuf {
        self.root.path().join("config")
    }

    pub(crate) fn state_home(&self) -> PathBuf {
        self.root.path().join("state")
    }

    pub(crate) fn tmpdir(&self) -> PathBuf {
        self.root.path().join("tmp")
    }

    /// Gives `command` this sandbox's environment and `cwd` as its working directory,
    /// dropping everything else it would inherit except `PATH`.
    pub(crate) fn isolate<'a>(&self, command: &'a mut Command, cwd: &Path) -> &'a mut Command {
        command
            .env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.config_home())
            .env("XDG_STATE_HOME", self.state_home())
            .env("TMPDIR", self.tmpdir())
            .current_dir(cwd);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command
    }

    /// Runs `ktask-rs` with `args` in `cwd` and waits for it to exit.
    pub(crate) fn run(&self, cwd: &Path, args: &[&str]) -> Result<Outcome> {
        self.run_with(cwd, args, |_| {})
    }

    /// Like [`Sandbox::run`], after `adjust` has changed the child's environment.
    pub(crate) fn run_with(
        &self,
        cwd: &Path,
        args: &[&str],
        adjust: impl FnOnce(&mut Command),
    ) -> Result<Outcome> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        self.isolate(&mut command, cwd);
        adjust(&mut command);
        let output = command.output()?;
        Ok(Outcome {
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
            code: output.status.code(),
        })
    }
}

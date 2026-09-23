//! Shared end-to-end harness for `ktask-rs` scenario tests (T114).
//!
//! [`build`] assembles a whole runnable project — a scratch git repository
//! with a local bare `origin` (see [`ktask_core::testing::scratch_repo`]),
//! registered with `ktask-rs init`, configured to drive the built-in
//! `dummy` provider against a caller-supplied scenario, and with a plan
//! already imported into its queue via `add --file` — entirely under a
//! fresh [`tempfile::TempDir`] rooted in the system temp directory. Nothing
//! it creates lives inside this repository or the developer's real
//! `XDG_STATE_HOME`/`XDG_CONFIG_HOME`, and dropping the returned [`Scenario`]
//! removes it all from disk.
//!
//! Setup failures are reported as `io::Error` rather than unwrapped here —
//! matching `ktask_core::testing`'s own fixtures — so the `#[test]`
//! functions that call [`build`] are the ones that `.expect()` it, keeping
//! panics where `clippy.toml`'s `allow-expect-in-tests` actually covers
//! them.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ktask_core::testing::{ScratchRepo, scratch_repo};

/// A disposable end-to-end environment: a scratch git project with a bare
/// `origin`, plus isolated `XDG_STATE_HOME`/`XDG_CONFIG_HOME` directories.
/// Everything lives under the system temp directory and is removed when
/// this value is dropped.
pub(crate) struct Scenario {
    repo: ScratchRepo,
    state_home: tempfile::TempDir,
    config_home: tempfile::TempDir,
}

impl Scenario {
    /// The scratch project's working directory — the repository `ktask-rs`
    /// operates on in this scenario.
    pub(crate) fn project_dir(&self) -> &Path {
        &self.repo.path
    }

    /// Runs the compiled `ktask-rs` binary with `args`, rooted at this
    /// scenario's project directory, with `XDG_STATE_HOME` and
    /// `XDG_CONFIG_HOME` pointed at this scenario's isolated, disposable
    /// directories — never the developer's real ones — and returns its
    /// captured stdout, stderr and exit code.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the compiled binary cannot be spawned.
    pub(crate) fn run(&self, args: &[&str]) -> io::Result<Output> {
        let exe = env!("CARGO_BIN_EXE_ktask-rs");
        Command::new(exe)
            .args(args)
            .current_dir(self.project_dir())
            .env("XDG_STATE_HOME", self.state_home.path())
            .env("XDG_CONFIG_HOME", self.config_home.path())
            .output()
    }
}

/// Builds a full [`Scenario`]: registers a scratch project with `ktask-rs
/// init`, writes a project config naming the `dummy` provider against
/// `scenario_toml` (see `ktask_core::provider::dummy::Scenario` for the
/// format), and imports `plan` into the (until now empty) queue via
/// `add --file`.
///
/// Every file this writes — the scenario, the config and the plan — lives
/// under the scenario's state home, never inside the project repository
/// itself, matching how ktask-rs keeps its own operational context out of
/// the repositories it supervises.
///
/// # Errors
///
/// Returns an `io::Error` naming the problem if any setup step fails: the
/// scratch repository or a temp directory could not be created, `init` or
/// `add --file` exited non-zero, or either subprocess's output was not
/// valid UTF-8.
pub(crate) fn build(scenario_toml: &str, plan: &str) -> io::Result<Scenario> {
    let repo = scratch_repo().map_err(io::Error::other)?;
    let state_home = tempfile::tempdir()?;
    let config_home = tempfile::tempdir()?;
    let scenario = Scenario {
        repo,
        state_home,
        config_home,
    };

    let init = scenario.run(&["init"])?;
    if !init.status.success() {
        return Err(io::Error::other(format!(
            "init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        )));
    }
    let stdout = String::from_utf8(init.stdout).map_err(io::Error::other)?;
    let state_dir: PathBuf = stdout
        .lines()
        .find_map(|line| line.strip_prefix("state: "))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("init did not report a state directory"))?;

    let scenario_path = state_dir.join("scenario.toml");
    std::fs::write(&scenario_path, scenario_toml)?;

    std::fs::write(
        state_dir.join("config.toml"),
        format!(
            "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\nverify_command = [\"true\"]\n",
            scenario_path.display()
        ),
    )?;

    let plan_path = state_dir.join("plan.md");
    std::fs::write(&plan_path, plan)?;
    let plan_path_str = plan_path
        .to_str()
        .ok_or_else(|| io::Error::other("plan path is not utf-8"))?;
    let add = scenario.run(&["add", "--file", plan_path_str])?;
    if !add.status.success() {
        return Err(io::Error::other(format!(
            "add --file failed: {}",
            String::from_utf8_lossy(&add.stderr)
        )));
    }

    Ok(scenario)
}

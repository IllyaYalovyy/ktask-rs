//! Black-box proof of the project-resolution rule `main` implements for
//! `docs/CONTRACT.md` section 2: every command needs an already-registered
//! project except `init`, which must work precisely when none exists yet.

use std::io;
use std::path::Path;
use std::process::{Command, Output};

fn ktask_rs(project_dir: &Path, state_home: &Path, args: &[&str]) -> io::Result<Output> {
    let exe = env!("CARGO_BIN_EXE_ktask-rs");
    Command::new(exe)
        .arg("--project")
        .arg(project_dir)
        .args(args)
        .env("XDG_STATE_HOME", state_home)
        .output()
}

/// `init` is the one command dispatch must reach with no project registered
/// yet: it exits 0 rather than falling into the generic "no project found"
/// usage error every other command hits in this situation.
#[test]
fn init_succeeds_with_no_project_registered_yet() {
    let project_dir = tempfile::tempdir().expect("project dir");
    let state_home = tempfile::tempdir().expect("state home");

    let output =
        ktask_rs(project_dir.path(), state_home.path(), &["init"]).expect("run ktask-rs init");

    assert_eq!(
        output.status.code(),
        Some(0),
        "init must succeed on an unregistered directory, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Once `init` has registered a project, a command that requires one finds
/// it via discovery and no longer hits the "no project found" usage error.
#[test]
fn a_command_after_init_finds_the_registered_project() {
    let project_dir = tempfile::tempdir().expect("project dir");
    let state_home = tempfile::tempdir().expect("state home");

    let init =
        ktask_rs(project_dir.path(), state_home.path(), &["init"]).expect("run ktask-rs init");
    assert_eq!(init.status.code(), Some(0), "init must succeed first");

    let status =
        ktask_rs(project_dir.path(), state_home.path(), &["status"]).expect("run ktask-rs status");
    assert_eq!(
        status.status.code(),
        Some(0),
        "status after init must find the registered project, stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// A command other than `init` run before any registration exits 2 and
/// names `ktask-rs init` as the remedy, per `docs/CONTRACT.md` section 2.
#[test]
fn a_command_before_init_reports_the_documented_usage_error() {
    let project_dir = tempfile::tempdir().expect("project dir");
    let state_home = tempfile::tempdir().expect("state home");

    let output =
        ktask_rs(project_dir.path(), state_home.path(), &["status"]).expect("run ktask-rs status");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(
        stderr.contains("ktask-rs init"),
        "expected the remedy to name `ktask-rs init`, got: {stderr:?}"
    );
}

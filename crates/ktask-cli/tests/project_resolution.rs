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

/// Initializes `dir` as a git repository, so it is eligible for
/// registration: `docs/CONTRACT.md` section 3 requires `init` to reject a
/// directory that is not one.
fn git_init(dir: &Path) -> io::Result<()> {
    let status = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(dir)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "git init failed for {}",
            dir.display()
        )))
    }
}

/// `init` is the one command dispatch must reach with no project registered
/// yet: it exits 0 rather than falling into the generic "no project found"
/// usage error every other command hits in this situation.
#[test]
fn init_succeeds_with_no_project_registered_yet() {
    let project_dir = tempfile::tempdir().expect("project dir");
    git_init(project_dir.path()).expect("git init");
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

/// `init` rejects a directory that is not a git repository: exit 2, a clear
/// message, and nothing created inside the directory itself (state lives
/// under `XDG_STATE_HOME`, never inside the repository).
#[test]
fn init_outside_a_git_repository_exits_2_and_creates_nothing_in_the_directory() {
    let project_dir = tempfile::tempdir().expect("project dir (deliberately not a git repo)");
    let state_home = tempfile::tempdir().expect("state home");

    let output =
        ktask_rs(project_dir.path(), state_home.path(), &["init"]).expect("run ktask-rs init");

    assert_eq!(
        output.status.code(),
        Some(2),
        "init outside a git repository must be a usage error, stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(
        stderr.contains("git"),
        "expected a message naming the git problem, got: {stderr:?}"
    );
    assert_eq!(
        std::fs::read_dir(project_dir.path())
            .expect("read project dir")
            .count(),
        0,
        "init must not create anything inside a directory it refused to register"
    );
}

/// Once `init` has registered a project, running it again against the same
/// directory reports the existing registration and still exits 0, per
/// `docs/CONTRACT.md` section 3's idempotence requirement.
#[test]
fn init_run_twice_reports_the_same_registration_and_exits_0_both_times() {
    let project_dir = tempfile::tempdir().expect("project dir");
    git_init(project_dir.path()).expect("git init");
    let state_home = tempfile::tempdir().expect("state home");

    let first =
        ktask_rs(project_dir.path(), state_home.path(), &["init"]).expect("run ktask-rs init");
    let second =
        ktask_rs(project_dir.path(), state_home.path(), &["init"]).expect("run ktask-rs init");

    assert_eq!(first.status.code(), Some(0), "first init must succeed");
    assert_eq!(second.status.code(), Some(0), "second init must succeed");

    let first_stdout = String::from_utf8(first.stdout).expect("stdout is utf8");
    let second_stdout = String::from_utf8(second.stdout).expect("stdout is utf8");
    assert_eq!(
        first_stdout, second_stdout,
        "re-running init must report the same registration"
    );
    assert!(
        first_stdout.contains("state:"),
        "expected the state directory to be reported, got: {first_stdout:?}"
    );
}

/// Once `init` has registered a project, a command that requires one finds
/// it via discovery and no longer hits the "no project found" usage error.
#[test]
fn a_command_after_init_finds_the_registered_project() {
    let project_dir = tempfile::tempdir().expect("project dir");
    git_init(project_dir.path()).expect("git init");
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

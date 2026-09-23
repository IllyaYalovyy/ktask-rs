//! Black-box proof of `docs/CONTRACT.md` section 2: the five global options
//! accepted by every command actually do what that section documents.
//! `ktask-cli` has no library target, so spawning the compiled binary is
//! the only way to observe `--project` overriding discovery of the real
//! process working directory, the real separate stdout/stderr streams
//! `--verbose`/`--quiet` govern, and the real process exit code
//! `--verbose`+`--quiet` together must produce.

use std::io;
use std::path::Path;
use std::process::{Command, Output};

/// Runs the compiled `ktask-rs` binary with `args`, rooted at `cwd` (the
/// process's real working directory — distinct from any `--project` flag
/// `args` may itself contain) and an isolated `XDG_STATE_HOME` so no real
/// registration on the machine running this test can be found.
fn ktask_rs_in(cwd: &Path, state_home: &Path, args: &[&str]) -> io::Result<Output> {
    let exe = env!("CARGO_BIN_EXE_ktask-rs");
    Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .env("XDG_STATE_HOME", state_home)
        .output()
}

/// Initializes `dir` as a git repository, the precondition `init` enforces
/// for registration.
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

/// `--project` must override discovery: a command run with its process
/// working directory somewhere that is *not* a registered project must
/// still find the project named by `--project`, and the same command with
/// `--project` omitted from that same working directory must fail to find
/// one.
#[test]
fn project_flag_overrides_discovery() {
    let project_dir = tempfile::tempdir().expect("project dir");
    git_init(project_dir.path()).expect("git init");
    let elsewhere = tempfile::tempdir().expect("an unrelated working directory");
    let state_home = tempfile::tempdir().expect("state home");

    let registered = ktask_rs_in(
        elsewhere.path(),
        state_home.path(),
        &[
            "--project",
            project_dir.path().to_str().expect("utf-8"),
            "init",
        ],
    )
    .expect("run init");
    assert_eq!(
        registered.status.code(),
        Some(0),
        "init must register the project named by --project, stderr: {}",
        String::from_utf8_lossy(&registered.stderr)
    );

    let with_override = ktask_rs_in(
        elsewhere.path(),
        state_home.path(),
        &[
            "--project",
            project_dir.path().to_str().expect("utf-8"),
            "status",
        ],
    )
    .expect("run status with --project");
    assert_eq!(
        with_override.status.code(),
        Some(0),
        "status --project must find the registered project even though the \
         process cwd is unrelated, stderr: {}",
        String::from_utf8_lossy(&with_override.stderr)
    );

    let without_override = ktask_rs_in(elsewhere.path(), state_home.path(), &["status"])
        .expect("run status without --project");
    assert_eq!(
        without_override.status.code(),
        Some(2),
        "status without --project must fall back to discovery from the \
         unrelated cwd and find nothing there"
    );
}

/// `--quiet` suppresses non-essential stderr (`progress`) while leaving a
/// command's result on stdout untouched.
#[test]
fn quiet_suppresses_progress_but_leaves_stdout_results_intact() {
    let project_dir = tempfile::tempdir().expect("an unregistered project directory");
    let state_home = tempfile::tempdir().expect("state home");

    let loud = ktask_rs_in(project_dir.path(), state_home.path(), &["--json", "status"])
        .expect("run status without --quiet");
    let quiet = ktask_rs_in(
        project_dir.path(),
        state_home.path(),
        &["--json", "--quiet", "status"],
    )
    .expect("run status with --quiet");

    assert_eq!(loud.status.code(), Some(2));
    assert_eq!(quiet.status.code(), Some(2));

    let loud_stderr = String::from_utf8(loud.stderr).expect("utf8");
    let quiet_stderr = String::from_utf8(quiet.stderr).expect("utf8");
    assert!(
        loud_stderr.contains("error:"),
        "expected the usage error on stderr without --quiet, got: {loud_stderr:?}"
    );
    assert!(
        quiet_stderr.is_empty(),
        "--quiet must suppress progress entirely, got: {quiet_stderr:?}"
    );

    let loud_stdout = String::from_utf8(loud.stdout).expect("utf8");
    let quiet_stdout = String::from_utf8(quiet.stdout).expect("utf8");
    assert_eq!(
        loud_stdout, quiet_stdout,
        "--quiet must never change the stdout result"
    );
    let result: serde_json::Value = serde_json::from_str(quiet_stdout.trim())
        .unwrap_or_else(|e| panic!("stdout did not parse as JSON: {e}\nstdout: {quiet_stdout:?}"));
    assert_eq!(result["outcome"], "usage");
}

/// `--verbose` adds diagnostic detail to stderr that is absent by default.
#[test]
fn verbose_adds_diagnostic_detail_to_stderr() {
    let project_dir = tempfile::tempdir().expect("an unregistered project directory");
    let state_home = tempfile::tempdir().expect("state home");

    let quiet_run =
        ktask_rs_in(project_dir.path(), state_home.path(), &["status"]).expect("run status");
    let verbose_run = ktask_rs_in(
        project_dir.path(),
        state_home.path(),
        &["--verbose", "status"],
    )
    .expect("run status --verbose");

    let default_stderr = String::from_utf8(quiet_run.stderr).expect("utf8");
    let verbose_stderr = String::from_utf8(verbose_run.stderr).expect("utf8");

    assert!(
        !default_stderr.contains("\"verbose\""),
        "diagnostic detail must not appear without --verbose, got: {default_stderr:?}"
    );
    assert!(
        verbose_stderr.contains("\"verbose\":true"),
        "expected the diagnostic dump under --verbose, got: {verbose_stderr:?}"
    );
}

/// `--json` switches a state-reporting outcome to its JSON form on stdout;
/// the human form never appears alongside it.
#[test]
fn json_switches_the_reported_outcome_to_its_json_form() {
    let project_dir = tempfile::tempdir().expect("an unregistered project directory");
    let state_home = tempfile::tempdir().expect("state home");

    let human = ktask_rs_in(project_dir.path(), state_home.path(), &["status"]).expect("run");
    let json = ktask_rs_in(project_dir.path(), state_home.path(), &["--json", "status"])
        .expect("run --json");

    let human_stderr = String::from_utf8(human.stderr).expect("utf8");
    let human_stdout = String::from_utf8(human.stdout).expect("utf8");
    let json_stdout = String::from_utf8(json.stdout).expect("utf8");

    assert!(
        human_stdout.is_empty(),
        "human-mode usage error must not print on stdout, got: {human_stdout:?}"
    );
    assert!(
        human_stderr.contains("error:"),
        "human-mode usage error must print on stderr, got: {human_stderr:?}"
    );

    let result: serde_json::Value = serde_json::from_str(json_stdout.trim())
        .unwrap_or_else(|e| panic!("stdout did not parse as JSON: {e}\nstdout: {json_stdout:?}"));
    assert_eq!(result["outcome"], "usage");
    assert!(
        result["detail"]
            .as_str()
            .expect("detail is a string")
            .contains("ktask-rs init")
    );
}

/// `--verbose` and `--quiet` set opposite ends of the same threshold;
/// requesting both is a usage error (exit 2), not a silently-resolved
/// combination.
#[test]
fn verbose_and_quiet_together_is_a_usage_error() {
    let project_dir = tempfile::tempdir().expect("project dir");
    let state_home = tempfile::tempdir().expect("state home");

    let output = ktask_rs_in(
        project_dir.path(),
        state_home.path(),
        &["--verbose", "--quiet", "status"],
    )
    .expect("run status --verbose --quiet");

    assert_eq!(
        output.status.code(),
        Some(2),
        "--verbose with --quiet must be a usage error"
    );
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(
        stderr.contains("--verbose") && stderr.contains("--quiet"),
        "expected a message naming both conflicting flags, got: {stderr:?}"
    );
}

/// The combined proof `docs/CONTRACT.md` section 2 asks for: every global
/// flag does what the contract says, exercised together against the
/// compiled binary rather than any one function in isolation.
#[test]
fn global_options_are_honoured() {
    let project_dir = tempfile::tempdir().expect("project dir");
    git_init(project_dir.path()).expect("git init");
    let elsewhere = tempfile::tempdir().expect("an unrelated working directory");
    let state_home = tempfile::tempdir().expect("state home");

    // `--project` overrides discovery: registering and then querying from
    // an unrelated cwd both succeed only because `--project` names the
    // project explicitly.
    let init = ktask_rs_in(
        elsewhere.path(),
        state_home.path(),
        &[
            "--project",
            project_dir.path().to_str().expect("utf-8"),
            "init",
        ],
    )
    .expect("run init");
    assert_eq!(init.status.code(), Some(0), "--project init must succeed");

    let status = ktask_rs_in(
        elsewhere.path(),
        state_home.path(),
        &[
            "--project",
            project_dir.path().to_str().expect("utf-8"),
            "--json",
            "--quiet",
            "status",
        ],
    )
    .expect("run status");
    assert_eq!(
        status.status.code(),
        Some(0),
        "--project must override discovery of the unrelated cwd, stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    // `--quiet` leaves the stdout result intact while suppressing stderr.
    let quiet_stderr = String::from_utf8(status.stderr).expect("utf8");
    assert!(
        quiet_stderr.is_empty(),
        "--quiet must suppress stderr progress, got: {quiet_stderr:?}"
    );

    // `--json` on a state-reporting outcome is the same JSON form proven
    // by `json_switches_the_reported_outcome_to_its_json_form`, exercised
    // here through the "no project" usage outcome from an unrelated cwd
    // with no `--project` override.
    let no_project_json = ktask_rs_in(elsewhere.path(), state_home.path(), &["--json", "status"])
        .expect("run status --json with no project");
    assert_eq!(no_project_json.status.code(), Some(2));
    let no_project_stdout = String::from_utf8(no_project_json.stdout).expect("utf8");
    let result: serde_json::Value =
        serde_json::from_str(no_project_stdout.trim()).unwrap_or_else(|e| {
            panic!("stdout did not parse as JSON: {e}\nstdout: {no_project_stdout:?}")
        });
    assert_eq!(result["outcome"], "usage");

    // `--verbose` adds diagnostic detail that is otherwise absent.
    let verbose = ktask_rs_in(
        elsewhere.path(),
        state_home.path(),
        &["--verbose", "status"],
    )
    .expect("run status --verbose");
    let verbose_stderr = String::from_utf8(verbose.stderr).expect("utf8");
    assert!(
        verbose_stderr.contains("\"verbose\":true"),
        "expected diagnostic detail under --verbose, got: {verbose_stderr:?}"
    );

    // `--verbose` and `--quiet` together is a usage error, not a silently
    // resolved combination.
    let conflict = ktask_rs_in(
        elsewhere.path(),
        state_home.path(),
        &["--verbose", "--quiet", "status"],
    )
    .expect("run status --verbose --quiet");
    assert_eq!(
        conflict.status.code(),
        Some(2),
        "--verbose with --quiet must be rejected as a usage error"
    );
}

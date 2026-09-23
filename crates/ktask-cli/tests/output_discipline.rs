//! Black-box proof of `docs/CONTRACT.md` section 0 rule 4 against the
//! compiled binary: `ktask-cli` has no library target, so this is the only
//! way to observe its real, separate stdout and stderr streams rather than
//! the `render`/`json` functions in isolation (covered by their own unit
//! tests).

use std::process::Command;

/// Runs `status --json --verbose` against a directory that is guaranteed not
/// to be a registered project (a fresh `tempfile::tempdir`, with
/// `XDG_STATE_HOME` pointed at another fresh directory so no real
/// registration on the machine running this test can be found either). Every
/// per-command `cmd::` implementation is still a placeholder (T107's own
/// scope; T111 gives `status` its real body), so this "no project" usage
/// error is the one outcome dispatch can produce on its own right now — and
/// it is enough to prove the stdout/stderr split `docs/CONTRACT.md` section 0
/// rule 4 requires: the JSON result lands only on stdout, the `--verbose`
/// diagnostic dump lands only on stderr, and neither leaks into the other.
#[test]
fn json_result_on_stdout_is_never_mixed_with_verbose_progress_on_stderr() {
    let exe = env!("CARGO_BIN_EXE_ktask-rs");
    let project_dir = tempfile::tempdir().expect("an unregistered project directory");
    let state_home = tempfile::tempdir().expect("an empty XDG_STATE_HOME");

    let output = Command::new(exe)
        .args([
            "--project",
            project_dir.path().to_str().expect("utf-8 path"),
            "status",
            "--json",
            "--verbose",
        ])
        .env("XDG_STATE_HOME", state_home.path())
        .output()
        .expect("run the compiled ktask-rs binary");

    assert_eq!(
        output.status.code(),
        Some(2),
        "no registered project is a usage error"
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");

    let result: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout did not parse as JSON: {e}\nstdout: {stdout:?}"));
    assert!(
        result.is_object(),
        "expected a JSON object result, got {result:?}"
    );
    assert_eq!(stdout.matches('\n').count(), 1, "result must be one line");

    // The `--verbose` dump serializes the parsed `Cli`, which names the
    // `status` subcommand distinctly from the `--json` result's own shape.
    assert!(
        stderr.contains("\"verbose\":true"),
        "expected the verbose diagnostic dump on stderr, got: {stderr:?}"
    );
    assert!(
        stderr.contains("ktask-rs init"),
        "expected the no-project error to name `ktask-rs init`, got: {stderr:?}"
    );
    assert!(
        !stdout.contains("\"verbose\""),
        "progress chatter leaked into the stdout result: {stdout:?}"
    );
    assert!(
        !stderr.contains("\"outcome\""),
        "the stdout result leaked into stderr: {stderr:?}"
    );
}

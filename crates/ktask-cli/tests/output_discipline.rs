//! Black-box proof of `docs/CONTRACT.md` section 0 rule 4 against the
//! compiled binary: `ktask-cli` has no library target, so this is the only
//! way to observe its real, separate stdout and stderr streams rather than
//! the `render`/`json` functions in isolation (covered by their own unit
//! tests).

use std::process::Command;

/// Runs `status --json --verbose`, a state-reporting invocation per
/// `docs/CONTRACT.md` section 3, and checks that the JSON result on stdout
/// parses cleanly while the `--verbose` diagnostic dump — real progress
/// chatter, not fixture text — lands only on stderr.
#[test]
fn json_result_on_stdout_is_never_mixed_with_verbose_progress_on_stderr() {
    let exe = env!("CARGO_BIN_EXE_ktask-rs");
    let output = Command::new(exe)
        .args(["status", "--json", "--verbose"])
        .output()
        .expect("run the compiled ktask-rs binary");

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
        !stdout.contains("\"verbose\""),
        "progress chatter leaked into the stdout result: {stdout:?}"
    );
    assert!(
        !stderr.contains("\"outcome\""),
        "the stdout result leaked into stderr: {stderr:?}"
    );
}

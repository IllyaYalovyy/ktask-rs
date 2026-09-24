//! `ktask-rs doctor`: environment and provider health checks with remedies.
//!
//! `docs/CONTRACT.md` section 3: five checks — provider availability, git,
//! toolchain, state directory permissions and journal health — each printed
//! as one line with a pass/fail status, a detail and, on failure, a remedy.
//! Exit 0 when every check passes, 1 otherwise (`RunOutcome::CheckFailed`,
//! wired to exit 1 in `crate::exit`). `--json` emits an array of
//! `{check, status, detail, remedy}`, one object per check.
//!
//! The checks themselves are `ktask_core::run_checks`, shared with the
//! interface's configuration screen so both word every remedy the same way;
//! [`run`] only prints them and turns them into an exit code.

use ktask_core::{CheckResult, Config, Project, RunOutcome, run_checks};

use crate::{json, render};

/// Runs every check against `project` and `config`, prints one line per
/// check (or, under `json`, one JSON array to stdout), and reports whether
/// all of them passed.
pub(crate) fn run(project: &Project, config: &Config, json_output: bool) -> RunOutcome {
    let results = run_checks(project, config);

    if json_output {
        let _ = json::emit_json(&results);
    } else {
        for result in &results {
            render::out(format_args!("{}", result.render_line()));
        }
    }

    if results.iter().all(CheckResult::passed) {
        RunOutcome::Drained
    } else {
        RunOutcome::CheckFailed {
            detail: failure_summary(&results),
        }
    }
}

/// Summarizes every failing check's name for [`RunOutcome::CheckFailed`].
fn failure_summary(results: &[CheckResult]) -> String {
    let failing: Vec<&str> = results
        .iter()
        .filter(|r| !r.passed())
        .map(|r| r.check)
        .collect();
    format!(
        "doctor: {} check(s) failed: {}",
        failing.len(),
        failing.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process::Command;

    fn config_with_provider(provider: &str) -> Config {
        let mut config = Config::default();
        config.provider = provider.to_string();
        config
    }

    // -- run --------------------------------------------------------------------

    /// A `Project` whose paths never exist, so every filesystem-backed
    /// check (`state_dir`, `journal`) genuinely fails without touching real
    /// state — the same fixture shape `cmd::mod`'s dispatch test uses.
    fn broken_project() -> Project {
        Project {
            root: std::path::PathBuf::from("/nonexistent/ktask-doctor-run-fixture/root"),
            id: "doctor-run-fixture".to_string(),
            state_dir: std::path::PathBuf::from("/nonexistent/ktask-doctor-run-fixture/state"),
        }
    }

    #[test]
    fn run_reports_check_failed_when_the_state_directory_is_missing() {
        let outcome = run(&broken_project(), &Config::default(), false);
        assert!(
            matches!(outcome, RunOutcome::CheckFailed { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn run_reports_drained_when_every_check_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scenario_path = dir.path().join("scenario.toml");
        std::fs::write(
            &scenario_path,
            "[[steps]]\noutcome = \"success\"\nstdout = \"ok\"\nexit_code = 0\n",
        )
        .expect("write scenario");

        let mut config = config_with_provider("dummy");
        config.dummy_scenario_path = Some(scenario_path);

        let project = Project {
            root: dir.path().to_path_buf(),
            id: "doctor-run-passing-fixture".to_string(),
            state_dir: dir.path().to_path_buf(),
        };

        // Relies on `git` and the Rust toolchain genuinely being on `PATH`,
        // which is guaranteed by this very test binary having been built
        // and this repository being a git checkout.
        let outcome = run(&project, &config, false);
        assert_eq!(outcome, RunOutcome::Drained, "{outcome:?}");
    }

    /// Spawns this test binary re-executed as `child_name`, the pattern
    /// `render.rs` and `json.rs` also use to observe real, separate stdout
    /// and stderr for a process boundary this crate has no library target
    /// to unit test against directly.
    fn run_child(child_name: &str) -> (String, String) {
        let exe = env::current_exe().expect("current test exe");
        let output = Command::new(exe)
            .args(["--exact", "--ignored", "--nocapture", child_name])
            .output()
            .expect("spawn child");
        (
            String::from_utf8(output.stdout).expect("stdout is utf8"),
            String::from_utf8(output.stderr).expect("stderr is utf8"),
        )
    }

    #[test]
    fn run_prints_one_line_per_check_to_stdout_only() {
        let (stdout, stderr) = run_child("cmd::doctor::tests::emit_fixture_doctor_run");

        assert!(
            stdout.contains("PASS") || stdout.contains("FAIL"),
            "{stdout:?}"
        );
        for check in ["provider", "git", "toolchain", "state_dir", "journal"] {
            assert!(stdout.contains(check), "missing {check} line: {stdout:?}");
        }
        assert!(
            !stderr.contains("PASS") && !stderr.contains("FAIL"),
            "check lines leaked onto stderr: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                run_prints_one_line_per_check_to_stdout_only"]
    fn emit_fixture_doctor_run() {
        run(&broken_project(), &Config::default(), false);
    }

    #[test]
    fn run_with_json_emits_one_array_to_stdout_with_the_documented_shape() {
        let (stdout, stderr) = run_child("cmd::doctor::tests::emit_fixture_doctor_run_json");

        let json_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with('[')).collect();
        assert_eq!(json_lines.len(), 1, "expected one JSON line: {stdout:?}");

        let parsed: serde_json::Value =
            serde_json::from_str(json_lines[0]).expect("valid JSON array");
        let array = parsed.as_array().expect("top-level value is an array");
        assert!(!array.is_empty());
        for entry in array {
            let obj = entry.as_object().expect("each entry is an object");
            for field in ["check", "status", "detail", "remedy"] {
                assert!(obj.contains_key(field), "missing field {field}: {entry:?}");
            }
        }
        assert!(
            !stderr.contains('['),
            "json output leaked onto stderr: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                run_with_json_emits_one_array_to_stdout_with_the_documented_shape"]
    fn emit_fixture_doctor_run_json() {
        run(&broken_project(), &Config::default(), true);
    }
}

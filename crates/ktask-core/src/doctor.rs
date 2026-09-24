//! The health checks behind `ktask-rs doctor`, shared by the command line and
//! the interface's configuration screen.
//!
//! `docs/CONTRACT.md` section 3: five checks — provider availability, git,
//! toolchain, state directory permissions and journal health — each with a
//! pass/fail status, a detail and, on failure, a remedy. They live here so the
//! two frontends cannot word a remedy differently: [`run_checks`] produces the
//! results and [`CheckResult::render_line`] the one line both print.
//!
//! Every `check_*` function is given its dependency on the outside world —
//! spawning a process, reading a directory — already reduced to a value or
//! a small closure, so a test can inject "git not found" or "the state
//! directory is not writable" without the real environment actually being
//! broken. [`run_checks`] alone wires each check to the real environment.

use crate::{Config, Journal, Project, journal_path};
use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// Whether a [`CheckResult`] passed or failed, per `docs/CONTRACT.md`
/// section 3's `--json` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// The check passed.
    Pass,
    /// The check failed; the paired [`CheckResult::remedy`] says how to fix
    /// it.
    Fail,
}

/// One check's outcome: `{check, status, detail, remedy}`, exactly the
/// shape `docs/CONTRACT.md` section 3 documents for `doctor --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    /// The check's name, stable across runs so a script can key on it.
    pub check: &'static str,
    /// Whether it passed.
    pub status: CheckStatus,
    /// Human-readable detail: what was found.
    pub detail: String,
    /// How to fix it, present only when `status` is [`CheckStatus::Fail`].
    pub remedy: Option<String>,
}

impl CheckResult {
    fn pass(check: &'static str, detail: impl Into<String>) -> Self {
        CheckResult {
            check,
            status: CheckStatus::Pass,
            detail: detail.into(),
            remedy: None,
        }
    }

    fn fail(check: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        CheckResult {
            check,
            status: CheckStatus::Fail,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }

    /// Whether the check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == CheckStatus::Pass
    }

    /// Renders one human-readable line: `docs/CONTRACT.md` section 3's
    /// "one line per check with pass/fail and a remedy for each failure".
    #[must_use]
    pub fn render_line(&self) -> String {
        let status = match self.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
        };
        match &self.remedy {
            Some(remedy) => format!(
                "{status} {}: {} (remedy: {remedy})",
                self.check, self.detail
            ),
            None => format!("{status} {}: {}", self.check, self.detail),
        }
    }
}

/// Runs every check against `project` and `config`, in the order the command
/// prints them.
#[must_use]
pub fn run_checks(project: &Project, config: &Config) -> Vec<CheckResult> {
    vec![
        check_provider(config, &probe_version),
        check_git(&|| probe_version("git")),
        check_toolchain(&|| probe_version("cargo"), &|| probe_version("rustc")),
        check_state_dir(&project.state_dir),
        check_journal(&project.state_dir),
    ]
}

/// Checks that the configured provider is actually runnable.
///
/// [`crate::build`] alone validates that `config.provider` names a
/// known adapter and, for `dummy`, that its scenario file is configured and
/// parses — so a `dummy` provider that builds needs no further check. For
/// an external adapter (`claude`, `codex`), `probe` reports the outcome of
/// running its command's version flag: `Some(detail)` when it ran,
/// `None` when it could not be spawned at all (most commonly: not
/// installed, not on `PATH`).
fn check_provider(config: &Config, probe: &dyn Fn(&str) -> Option<String>) -> CheckResult {
    if let Err(err) = crate::build(config) {
        return CheckResult::fail(
            "provider",
            err.to_string(),
            "fix `provider` (and, for `dummy`, `dummy_scenario_path`) in the project or global config",
        );
    }

    if config.provider == "dummy" {
        return CheckResult::pass(
            "provider",
            "dummy provider is configured and its scenario parses",
        );
    }

    match probe(&config.provider) {
        Some(detail) => CheckResult::pass(
            "provider",
            format!("{} is runnable: {detail}", config.provider),
        ),
        None => CheckResult::fail(
            "provider",
            format!("`{}` could not be run", config.provider),
            format!(
                "install the `{}` CLI and ensure it is on PATH",
                config.provider
            ),
        ),
    }
}

/// Checks that `git` is runnable.
fn check_git(probe: &dyn Fn() -> Option<String>) -> CheckResult {
    match probe() {
        Some(detail) => CheckResult::pass("git", detail),
        None => CheckResult::fail(
            "git",
            "`git` could not be run",
            "install git and ensure it is on PATH",
        ),
    }
}

/// Checks that the Rust toolchain (`cargo` and `rustc`) is runnable.
fn check_toolchain(
    cargo_probe: &dyn Fn() -> Option<String>,
    rustc_probe: &dyn Fn() -> Option<String>,
) -> CheckResult {
    match (cargo_probe(), rustc_probe()) {
        (Some(cargo), Some(rustc)) => CheckResult::pass("toolchain", format!("{cargo}; {rustc}")),
        (cargo, rustc) => {
            let missing: Vec<&str> = [
                cargo.is_none().then_some("cargo"),
                rustc.is_none().then_some("rustc"),
            ]
            .into_iter()
            .flatten()
            .collect();
            CheckResult::fail(
                "toolchain",
                format!("missing: {}", missing.join(", ")),
                "install the Rust toolchain (rustup) and ensure cargo and rustc are on PATH",
            )
        }
    }
}

/// Checks that `state_dir` exists and is writable, without leaving anything
/// behind: it creates and immediately removes a probe file.
fn check_state_dir(state_dir: &Path) -> CheckResult {
    if !state_dir.is_dir() {
        return CheckResult::fail(
            "state_dir",
            format!("state directory does not exist: {}", state_dir.display()),
            "run `ktask-rs init` to create it",
        );
    }

    let probe_path = state_dir.join(".doctor-write-probe");
    match std::fs::write(&probe_path, []) {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe_path);
            CheckResult::pass("state_dir", format!("{} is writable", state_dir.display()))
        }
        Err(err) => CheckResult::fail(
            "state_dir",
            format!("{} is not writable: {err}", state_dir.display()),
            format!(
                "fix permissions on {} so ktask-rs can write to it",
                state_dir.display()
            ),
        ),
    }
}

/// Checks that the project's journal opens and its events can be read back.
fn check_journal(state_dir: &Path) -> CheckResult {
    let path = journal_path(state_dir);
    match Journal::open(&path) {
        Ok(journal) => match journal.events() {
            Ok(events) => CheckResult::pass(
                "journal",
                format!(
                    "journal at {} opened, {} event(s) recorded",
                    path.display(),
                    events.len()
                ),
            ),
            Err(err) => CheckResult::fail(
                "journal",
                format!(
                    "journal at {} opened but could not be read: {err}",
                    path.display()
                ),
                "the journal file is corrupt; restore it from backup",
            ),
        },
        Err(err) => CheckResult::fail(
            "journal",
            format!("journal at {} did not open: {err}", path.display()),
            "the journal file is corrupt; restore it from backup",
        ),
    }
}

/// Runs `cmd --version` and returns its trimmed output, or `None` if `cmd`
/// could not be spawned at all (not installed, not on `PATH`).
///
/// Whichever of stdout/stderr `cmd` actually wrote its version to is used:
/// some tools (notably `rustc`) print `--version` to stdout, but a
/// nonexistent subcommand or flag error would land on stderr, and this
/// still wants to surface *something* rather than silently prefer an empty
/// stream.
fn probe_version(cmd: &str) -> Option<String> {
    let output = Command::new(cmd).arg("--version").output().ok()?;
    let bytes = if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_provider(provider: &str) -> Config {
        let mut config = Config::default();
        config.provider = provider.to_string();
        config
    }

    // -- check_provider -----------------------------------------------------

    #[test]
    fn check_provider_passes_for_dummy_with_a_valid_scenario() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scenario.toml");
        std::fs::write(
            &path,
            "[[steps]]\noutcome = \"success\"\nstdout = \"ok\"\nexit_code = 0\n",
        )
        .expect("write scenario");

        let mut config = config_with_provider("dummy");
        config.dummy_scenario_path = Some(path);

        let result = check_provider(&config, &|_| None);
        assert!(result.passed(), "{result:?}");
        assert!(result.remedy.is_none());
    }

    #[test]
    fn check_provider_fails_for_dummy_without_a_scenario_path() {
        let config = config_with_provider("dummy");

        let result = check_provider(&config, &|_| None);
        assert!(!result.passed());
        assert!(result.detail.contains("dummy_scenario_path"), "{result:?}");
        assert!(result.remedy.is_some());
    }

    #[test]
    fn check_provider_fails_for_an_unknown_provider_name() {
        let config = config_with_provider("not-a-real-provider");

        let result = check_provider(&config, &|_| None);
        assert!(!result.passed());
        assert!(result.detail.contains("not-a-real-provider"), "{result:?}");
    }

    #[test]
    fn check_provider_passes_for_claude_when_the_binary_is_runnable() {
        let config = config_with_provider("claude");

        let result = check_provider(&config, &|cmd| {
            assert_eq!(cmd, "claude");
            Some("claude-cli 1.2.3".to_string())
        });
        assert!(result.passed(), "{result:?}");
        assert!(result.detail.contains("claude-cli 1.2.3"));
    }

    #[test]
    fn check_provider_fails_for_claude_when_the_binary_cannot_be_run() {
        let config = config_with_provider("claude");

        let result = check_provider(&config, &|_| None);
        assert!(!result.passed());
        assert!(result.detail.contains("claude"), "{result:?}");
        assert!(
            result
                .remedy
                .as_deref()
                .unwrap_or_default()
                .contains("PATH")
        );
    }

    // -- check_git ------------------------------------------------------------

    #[test]
    fn check_git_passes_when_the_probe_reports_a_version() {
        let result = check_git(&|| Some("git version 2.45.0".to_string()));
        assert!(result.passed());
        assert_eq!(result.detail, "git version 2.45.0");
        assert!(result.remedy.is_none());
    }

    #[test]
    fn check_git_fails_when_the_probe_reports_nothing() {
        let result = check_git(&|| None);
        assert!(!result.passed());
        assert!(result.remedy.is_some());
    }

    // -- check_toolchain --------------------------------------------------------

    #[test]
    fn check_toolchain_passes_when_both_cargo_and_rustc_are_runnable() {
        let result = check_toolchain(&|| Some("cargo 1.90.0".to_string()), &|| {
            Some("rustc 1.90.0".to_string())
        });
        assert!(result.passed(), "{result:?}");
        assert!(result.detail.contains("cargo 1.90.0"));
        assert!(result.detail.contains("rustc 1.90.0"));
    }

    #[test]
    fn check_toolchain_fails_and_names_what_is_missing_when_cargo_is_absent() {
        let result = check_toolchain(&|| None, &|| Some("rustc 1.90.0".to_string()));
        assert!(!result.passed());
        assert!(result.detail.contains("cargo"), "{result:?}");
        assert!(!result.detail.contains("rustc"), "{result:?}");
    }

    #[test]
    fn check_toolchain_fails_and_names_both_when_neither_is_runnable() {
        let result = check_toolchain(&|| None, &|| None);
        assert!(!result.passed());
        assert!(result.detail.contains("cargo"));
        assert!(result.detail.contains("rustc"));
    }

    // -- check_state_dir --------------------------------------------------------

    #[test]
    fn check_state_dir_passes_for_a_writable_directory() {
        let dir = tempfile::tempdir().expect("tempdir");

        let result = check_state_dir(dir.path());
        assert!(result.passed(), "{result:?}");
        assert!(
            !dir.path().join(".doctor-write-probe").exists(),
            "the write probe must not be left behind"
        );
    }

    #[test]
    fn check_state_dir_fails_when_the_directory_does_not_exist() {
        let result = check_state_dir(Path::new("/nonexistent/ktask-doctor-fixture"));
        assert!(!result.passed());
        assert!(
            result
                .remedy
                .as_deref()
                .unwrap_or_default()
                .contains("init")
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_state_dir_fails_when_the_directory_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))
            .expect("make read-only");

        let result = check_state_dir(dir.path());

        // Restore write access so the tempdir can clean itself up.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore permissions");

        assert!(!result.passed(), "{result:?}");
    }

    // -- check_journal --------------------------------------------------------

    #[test]
    fn check_journal_passes_for_a_fresh_state_directory() {
        let dir = tempfile::tempdir().expect("tempdir");

        let result = check_journal(dir.path());
        assert!(result.passed(), "{result:?}");
        assert!(result.detail.contains("0 event"));
    }

    #[test]
    fn check_journal_fails_for_a_corrupt_journal_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(journal_path(dir.path()), b"not a sqlite database").expect("write garbage");

        let result = check_journal(dir.path());
        assert!(!result.passed());
        assert!(result.remedy.is_some());
    }

    // -- probe_version --------------------------------------------------------

    #[test]
    fn probe_version_returns_none_for_a_nonexistent_command() {
        assert_eq!(probe_version("ktask-doctor-nonexistent-command"), None);
    }

    #[test]
    fn probe_version_returns_output_for_a_real_command() {
        // `cargo` is guaranteed present: this test binary was built by it.
        let detail = probe_version("cargo").expect("cargo must be runnable while running tests");
        assert!(detail.to_lowercase().contains("cargo"), "{detail:?}");
    }

    // -- render_line ------------------------------------------------------------

    #[test]
    fn render_line_of_a_pass_has_no_remedy() {
        let line = CheckResult::pass("git", "git version 2.45.0").render_line();
        assert_eq!(line, "PASS git: git version 2.45.0");
    }

    #[test]
    fn render_line_of_a_failure_ends_with_its_remedy() {
        let line = CheckResult::fail("git", "`git` could not be run", "install git").render_line();
        assert_eq!(
            line,
            "FAIL git: `git` could not be run (remedy: install git)"
        );
    }

    // -- run_checks -------------------------------------------------------------

    #[test]
    fn run_checks_reports_the_five_checks_in_the_order_the_command_prints_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Project {
            root: dir.path().to_path_buf(),
            id: "doctor-order-fixture".to_string(),
            state_dir: dir.path().to_path_buf(),
        };
        let names: Vec<&str> = run_checks(&project, &Config::default())
            .iter()
            .map(|result| result.check)
            .collect();
        assert_eq!(
            names,
            ["provider", "git", "toolchain", "state_dir", "journal"]
        );
    }

    #[test]
    fn run_checks_fails_the_state_checks_of_a_project_whose_state_is_missing() {
        let project = Project {
            root: std::path::PathBuf::from("/nonexistent/ktask-doctor-checks-fixture/root"),
            id: "doctor-checks-fixture".to_string(),
            state_dir: std::path::PathBuf::from("/nonexistent/ktask-doctor-checks-fixture/state"),
        };
        let results = run_checks(&project, &Config::default());
        let failed: Vec<&str> = results
            .iter()
            .filter(|result| !result.passed())
            .map(|result| result.check)
            .collect();
        assert!(failed.contains(&"state_dir"), "{results:?}");
        assert!(failed.contains(&"journal"), "{results:?}");
    }
}

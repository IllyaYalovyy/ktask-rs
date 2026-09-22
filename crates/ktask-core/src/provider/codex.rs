//! [`Codex`]: the [`Provider`] adapter that drives the Codex CLI, on top of
//! [`super::run_streaming`] (`VISION.md` §12).
//!
//! Every invocation runs the configured command (its first element the
//! program, the rest fixed leading arguments — the same shape
//! [`crate::gate::Gate::command`] already uses) followed by this adapter's
//! own flags:
//! `exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir>`,
//! then `--model <model>` when [`Invocation::model`] names one, then a
//! trailing `-` that tells Codex to read the prompt from stdin rather than
//! argv — so the prompt never shows up in a process listing.
//!
//! A command that cannot be spawned at all — most commonly because the
//! configured program is not installed or not on `PATH` — surfaces as
//! [`Error::Provider`] exactly as [`super::run_streaming`]'s own
//! `a_nonexistent_program_is_a_provider_error` test documents. That is the
//! shape a misconfigured provider takes: `classify()` (`docs/DESIGN.md`,
//! not yet implemented) is what will map it to
//! [`crate::FailureClass::ProviderConfiguration`]; this module's job ends at
//! producing a `Result` that names the command and says plainly that it
//! could not be started.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::{Bus, Capabilities, Error, Invocation, Outcome, Provider, Result};

use super::run_streaming;

/// The flags this adapter always passes, before an optional `--model` and
/// the trailing `-` that puts the prompt on stdin.
const FIXED_ARGS: [&str; 3] = [
    "exec",
    "--dangerously-bypass-approvals-and-sandbox",
    "--skip-git-repo-check",
];

/// Builds the argument vector appended after the configured command's own
/// leading arguments: the fixed flags every invocation uses, `-C <dir>`,
/// `--model <model>` when `model` is given, and a trailing `-` so the
/// prompt is read from stdin.
///
/// Pure and spawns nothing, so it is exercised by unit tests directly.
fn codex_args(working_dir: &Path, model: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = FIXED_ARGS.iter().map(|&flag| flag.to_string()).collect();
    args.push("-C".to_string());
    args.push(working_dir.to_string_lossy().into_owned());
    if let Some(model) = model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.push("-".to_string());
    args
}

/// The [`Provider`] adapter for the Codex CLI.
///
/// `command` names the executable to run, exactly like
/// [`crate::gate::Gate::command`]: its first element is the program, and any
/// further elements are fixed leading arguments run ahead of this adapter's
/// own flags. `idle_timeout` and `hard_timeout` are passed straight through
/// to [`super::run_streaming`].
#[derive(Debug, Clone)]
pub struct Codex {
    command: Vec<String>,
    idle_timeout: Duration,
    hard_timeout: Duration,
}

impl Codex {
    /// Builds a `Codex` adapter that runs `command`, bounded by
    /// `idle_timeout` and `hard_timeout` on every invocation.
    #[must_use]
    pub fn new(command: Vec<String>, idle_timeout: Duration, hard_timeout: Duration) -> Self {
        Codex {
            command,
            idle_timeout,
            hard_timeout,
        }
    }
}

impl Provider for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    /// Codex accepts a specific model via `--model`; structured output and
    /// usage telemetry are not yet parsed out of its run, so both report
    /// `false` rather than a capability this adapter cannot actually serve.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_output: false,
            model_selection: true,
            usage_telemetry: false,
        }
    }

    /// Runs the configured command with this adapter's fixed flags,
    /// `-C <dir>`, and `--model` when `inv.model` names one, writing
    /// `inv.prompt` to its stdin behind a trailing `-`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Provider`] when the configured command is empty, or
    /// as [`super::run_streaming`] — most notably when the program could
    /// not be spawned at all (see the module doc).
    fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
        let (program, leading_args) =
            self.command.split_first().ok_or_else(|| Error::Provider {
                provider: "codex".to_string(),
                detail: "the configured command is empty".to_string(),
            })?;

        let mut cmd = Command::new(program);
        cmd.args(leading_args)
            .args(codex_args(&inv.working_dir, inv.model.as_deref()))
            .current_dir(&inv.working_dir);

        run_streaming(
            &mut cmd,
            Some(&inv.prompt),
            self.idle_timeout,
            self.hard_timeout,
            bus,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod codex_args {
        use super::*;

        #[test]
        fn with_no_model_the_fixed_flags_dash_c_and_a_trailing_dash_are_present() {
            assert_eq!(
                codex_args(Path::new("/work/dir"), None),
                vec![
                    "exec",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--skip-git-repo-check",
                    "-C",
                    "/work/dir",
                    "-",
                ]
            );
        }

        #[test]
        fn a_model_is_inserted_between_dash_c_and_the_trailing_dash() {
            assert_eq!(
                codex_args(Path::new("/work/dir"), Some("gpt-5-codex")),
                vec![
                    "exec",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--skip-git-repo-check",
                    "-C",
                    "/work/dir",
                    "--model",
                    "gpt-5-codex",
                    "-",
                ]
            );
        }
    }

    fn codex(command: &str) -> Codex {
        Codex::new(
            vec![command.to_string()],
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
    }

    fn invocation(prompt: &str, working_dir: &Path, model: Option<&str>) -> Invocation {
        Invocation {
            prompt: prompt.to_string(),
            model: model.map(str::to_string),
            working_dir: working_dir.to_path_buf(),
        }
    }

    #[test]
    fn name_reports_codex() {
        assert_eq!(codex("codex").name(), "codex");
    }

    #[test]
    fn capabilities_reports_model_selection_but_nothing_else() {
        let caps = codex("codex").capabilities();
        assert!(!caps.structured_output);
        assert!(caps.model_selection);
        assert!(!caps.usage_telemetry);
    }

    /// The core of `Done-when`: a command that cannot be spawned at all —
    /// standing in for the Codex CLI not being installed — is reported as
    /// [`Error::Provider`], the shape `classify()` will later map to
    /// [`crate::FailureClass::ProviderConfiguration`] (see the module doc).
    #[test]
    fn invoking_a_missing_executable_is_a_provider_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = codex("ktask-codex-test-nonexistent-binary");

        let err = provider
            .invoke(&invocation("do the thing", dir.path(), None), None)
            .expect_err("a nonexistent program must not be spawnable");

        let Error::Provider { provider, detail } = &err else {
            panic!("expected Error::Provider, got {err:?}");
        };
        assert_eq!(provider, "ktask-codex-test-nonexistent-binary");
        assert!(
            detail.contains("ktask-codex-test-nonexistent-binary"),
            "detail was: {detail}"
        );
    }

    #[test]
    fn an_empty_configured_command_is_a_provider_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = Codex::new(Vec::new(), Duration::from_secs(5), Duration::from_secs(5));

        let err = provider
            .invoke(&invocation("go", dir.path(), None), None)
            .expect_err("an empty command must be rejected before spawning");

        let Error::Provider { detail, .. } = &err else {
            panic!("expected Error::Provider, got {err:?}");
        };
        assert!(detail.contains("empty"), "detail was: {detail}");
    }

    /// Writes an executable shell script that echoes its own argv and the
    /// stdin it was given, in that order, then exits `0`. Standing in for
    /// the real Codex CLI, it lets `invoke` be proven end-to-end — argv
    /// built correctly, run from the right directory, prompt piped on
    /// stdin — without spawning the real, network-calling binary.
    fn recording_script(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("fake-codex.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf 'ARGS:%s\\n' \"$*\"\nprintf 'STDIN:'\ncat\n",
        )
        .expect("write fake codex script");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod script");
        }
        path
    }

    #[test]
    fn invoke_runs_the_configured_command_with_fixed_flags_and_pipes_the_prompt_on_stdin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = recording_script(dir.path());
        let provider = Codex::new(
            vec![script.to_string_lossy().into_owned()],
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        let outcome = provider
            .invoke(&invocation("hello from the prompt", dir.path(), None), None)
            .expect("invoke");

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            outcome.stdout,
            format!(
                "ARGS:exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C {} -\nSTDIN:hello from the prompt",
                dir.path().to_string_lossy()
            )
        );
    }

    #[test]
    fn invoke_adds_a_model_flag_when_the_invocation_names_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = recording_script(dir.path());
        let provider = Codex::new(
            vec![script.to_string_lossy().into_owned()],
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        let outcome = provider
            .invoke(&invocation("go", dir.path(), Some("gpt-5-codex")), None)
            .expect("invoke");

        assert_eq!(
            outcome.stdout,
            format!(
                "ARGS:exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C {} --model gpt-5-codex -\nSTDIN:go",
                dir.path().to_string_lossy()
            )
        );
    }

    #[test]
    fn invoke_omits_the_model_flag_when_no_model_is_requested() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = recording_script(dir.path());
        let provider = Codex::new(
            vec![script.to_string_lossy().into_owned()],
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        let outcome = provider
            .invoke(&invocation("go", dir.path(), None), None)
            .expect("invoke");

        assert!(!outcome.stdout.contains("--model"), "{}", outcome.stdout);
    }

    #[test]
    fn invoke_runs_leading_configured_arguments_before_the_fixed_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = recording_script(dir.path());
        let provider = Codex::new(
            vec![
                script.to_string_lossy().into_owned(),
                "--verbose".to_string(),
            ],
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        let outcome = provider
            .invoke(&invocation("go", dir.path(), None), None)
            .expect("invoke");

        assert_eq!(
            outcome.stdout,
            format!(
                "ARGS:--verbose exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C {} -\nSTDIN:go",
                dir.path().to_string_lossy()
            )
        );
    }

    #[test]
    fn invoke_runs_in_the_invocations_working_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir).expect("create subdir");
        let script_path = dir.path().join("fake-codex-pwd.sh");
        std::fs::write(&script_path, "#!/bin/sh\npwd\ncat >/dev/null\n").expect("write script");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).expect("chmod script");
        }
        let provider = Codex::new(
            vec![script_path.to_string_lossy().into_owned()],
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        let outcome = provider
            .invoke(&invocation("go", &subdir, None), None)
            .expect("invoke");

        let canonical_subdir = subdir.canonicalize().expect("canonicalize");
        let reported: std::path::PathBuf = outcome.stdout.trim().into();
        assert_eq!(
            reported.canonicalize().expect("canonicalize reported pwd"),
            canonical_subdir
        );
    }
}

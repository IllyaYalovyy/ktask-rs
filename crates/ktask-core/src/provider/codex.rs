//! Codex provider adapter.
//!
//! Provides an implementation of the Provider trait for the Codex CLI.
//! Invokes Codex with `exec --dangerously-bypass-approvals-and-sandbox
//! --skip-git-repo-check -C <dir>`, optionally adds `--model` when model
//! selection is requested, and passes the prompt on stdin via the trailing `-`.

use crate::provider::{Capabilities, Invocation, Outcome, Provider};
use crate::{Bus, Result};
use std::process::Command;
use std::time::Duration;

/// A provider that invokes Codex via the CLI.
///
/// Codex is invoked with `exec --dangerously-bypass-approvals-and-sandbox
/// --skip-git-repo-check -C <dir>`, the prompt is passed on stdin via `-`,
/// and model selection is supported.
#[derive(Debug)]
pub struct Codex {
    command: String,
}

impl Codex {
    /// Create a new Codex provider with the given command.
    ///
    /// # Arguments
    ///
    /// * `command` - The command to invoke for Codex (usually "codex")
    #[must_use]
    pub fn new(command: String) -> Self {
        Codex { command }
    }

    /// Build the argument vector for invoking Codex.
    ///
    /// This is a pure function that constructs the complete argument list
    /// without executing anything, making it easy to test.
    ///
    /// Arguments:
    /// - Always includes: `exec --dangerously-bypass-approvals-and-sandbox
    ///   --skip-git-repo-check -C <dir> -`
    /// - If model is Some: adds `--model <model>` after `-C <dir>`
    ///
    /// # Arguments
    ///
    /// * `dir` - Working directory to pass to `-C`
    /// * `model` - Optional model selection
    ///
    /// # Returns
    ///
    /// A vector of arguments suitable for `Command::args()`.
    fn build_args(dir: &str, model: Option<&str>) -> Vec<String> {
        let mut args = vec![
            "exec".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "--skip-git-repo-check".to_string(),
            "-C".to_string(),
            dir.to_string(),
        ];

        if let Some(m) = model {
            args.push("--model".to_string());
            args.push(m.to_string());
        }

        args.push("-".to_string());

        args
    }
}

impl Provider for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_output: false,
            model_selection: true,
            usage_telemetry: false,
        }
    }

    fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
        let mut cmd = Command::new(&self.command);

        // Build arguments from the working directory and model selection
        let dir_str = inv.working_dir.to_str().unwrap_or(".").to_string();
        let args = Self::build_args(&dir_str, inv.model.as_deref());
        cmd.args(args);

        // Execute with timeouts suitable for Codex
        let idle_timeout = Duration::from_secs(120);
        let hard_timeout = Duration::from_secs(600);

        crate::provider::run_streaming(&mut cmd, Some(&inv.prompt), idle_timeout, hard_timeout, bus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_without_model() {
        let args = Codex::build_args("/tmp/work", None);
        assert_eq!(args.len(), 6);
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "--dangerously-bypass-approvals-and-sandbox");
        assert_eq!(args[2], "--skip-git-repo-check");
        assert_eq!(args[3], "-C");
        assert_eq!(args[4], "/tmp/work");
        assert_eq!(args[5], "-");
    }

    #[test]
    fn build_args_with_model() {
        let args = Codex::build_args("/tmp/work", Some("codex-4"));
        assert_eq!(args.len(), 8);
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "--dangerously-bypass-approvals-and-sandbox");
        assert_eq!(args[2], "--skip-git-repo-check");
        assert_eq!(args[3], "-C");
        assert_eq!(args[4], "/tmp/work");
        assert_eq!(args[5], "--model");
        assert_eq!(args[6], "codex-4");
        assert_eq!(args[7], "-");
    }

    #[test]
    fn build_args_with_empty_model_string() {
        let args = Codex::build_args("/tmp/work", Some(""));
        assert_eq!(args.len(), 8);
        assert_eq!(args[5], "--model");
        assert_eq!(args[6], "");
        assert_eq!(args[7], "-");
    }

    #[test]
    fn build_args_with_special_characters_in_dir() {
        let args = Codex::build_args("/tmp/work with spaces", None);
        assert_eq!(args.len(), 6);
        assert_eq!(args[4], "/tmp/work with spaces");
        assert_eq!(args[5], "-");
    }

    #[test]
    fn codex_name_is_codex() {
        let codex = Codex {
            command: "codex".to_string(),
        };
        assert_eq!(codex.name(), "codex");
    }

    #[test]
    fn codex_capabilities_has_model_selection() {
        let codex = Codex {
            command: "codex".to_string(),
        };
        let caps = codex.capabilities();
        assert!(caps.model_selection);
        assert!(!caps.structured_output);
        assert!(!caps.usage_telemetry);
    }
}

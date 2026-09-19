//! Claude provider adapter.
//!
//! Provides an implementation of the Provider trait for the Claude CLI.
//! Invokes Claude with `--print --permission-mode bypassPermissions`,
//! and optionally `--model` when model selection is requested.

use crate::provider::{Capabilities, Invocation, Outcome, Provider};
use crate::{Bus, Result};
use std::process::Command;
use std::time::Duration;

/// A provider that invokes Claude via the CLI.
///
/// Claude is invoked with `--print --permission-mode bypassPermissions`,
/// the prompt is passed on stdin, and model selection is supported.
#[derive(Debug)]
pub struct Claude {
    command: String,
}

impl Claude {
    /// Create a new Claude provider with the given command.
    ///
    /// # Arguments
    ///
    /// * `command` - The command to invoke for Claude (usually "claude")
    #[must_use]
    pub fn new(command: String) -> Self {
        Claude { command }
    }

    /// Build the argument vector for invoking Claude.
    ///
    /// This is a pure function that constructs the complete argument list
    /// without executing anything, making it easy to test.
    ///
    /// Arguments:
    /// - Always includes: `--print --permission-mode bypassPermissions`
    /// - If model is Some: adds `--model <model>`
    ///
    /// # Arguments
    ///
    /// * `model` - Optional model selection
    ///
    /// # Returns
    ///
    /// A vector of arguments suitable for `Command::args()`.
    fn build_args(model: Option<&str>) -> Vec<String> {
        let mut args = vec![
            "--print".to_string(),
            "--permission-mode".to_string(),
            "bypassPermissions".to_string(),
        ];

        if let Some(m) = model {
            args.push("--model".to_string());
            args.push(m.to_string());
        }

        args
    }
}

impl Provider for Claude {
    fn name(&self) -> &'static str {
        "claude"
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

        // Build arguments from the model selection
        let args = Self::build_args(inv.model.as_deref());
        cmd.args(args);

        // Set working directory
        cmd.current_dir(&inv.working_dir);

        // Execute with timeouts suitable for Claude
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
        let args = Claude::build_args(None);
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], "--print");
        assert_eq!(args[1], "--permission-mode");
        assert_eq!(args[2], "bypassPermissions");
    }

    #[test]
    fn build_args_with_model() {
        let args = Claude::build_args(Some("claude-opus-5"));
        assert_eq!(args.len(), 5);
        assert_eq!(args[0], "--print");
        assert_eq!(args[1], "--permission-mode");
        assert_eq!(args[2], "bypassPermissions");
        assert_eq!(args[3], "--model");
        assert_eq!(args[4], "claude-opus-5");
    }

    #[test]
    fn build_args_with_empty_model_string() {
        let args = Claude::build_args(Some(""));
        assert_eq!(args.len(), 5);
        assert_eq!(args[3], "--model");
        assert_eq!(args[4], "");
    }

    #[test]
    fn claude_name_is_claude() {
        let claude = Claude {
            command: "claude".to_string(),
        };
        assert_eq!(claude.name(), "claude");
    }

    #[test]
    fn claude_capabilities_has_model_selection() {
        let claude = Claude {
            command: "claude".to_string(),
        };
        let caps = claude.capabilities();
        assert!(caps.model_selection);
        assert!(!caps.structured_output);
        assert!(!caps.usage_telemetry);
    }
}

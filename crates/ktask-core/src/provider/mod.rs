//! Provider types for token and cost reporting and a stable capability interface.

use crate::{Bus, Result};
use std::path::PathBuf;

/// Token and cost usage types.
pub mod usage;

/// Dummy provider for deterministic testing with scenario-driven outcomes.
pub mod dummy;

/// Process execution with streaming output and timeout management.
pub mod process;

/// Claude CLI provider adapter.
pub mod claude;

/// Codex CLI provider adapter.
pub mod codex;

/// Provider conformance test suite.
pub mod conformance;

pub use claude::Claude;
pub use codex::Codex;
pub use conformance::conformance_suite;
pub use dummy::{Dummy, Scenario, Step, StepOutcome};
pub use process::run_streaming;
pub use usage::{Usage, UsageSource};

/// Capabilities of a provider.
///
/// Describes what features a provider supports, allowing the core to adapt its
/// behavior and reporting based on the provider's capabilities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// Whether the provider supports structured output.
    pub structured_output: bool,
    /// Whether the provider supports model selection.
    pub model_selection: bool,
    /// Whether the provider supports usage telemetry.
    pub usage_telemetry: bool,
}

/// An invocation request for a provider.
///
/// Contains the prompt, optional model selection, and working directory
/// for the provider to execute.
#[derive(Clone, Debug)]
pub struct Invocation {
    /// The prompt to send to the provider.
    pub prompt: String,
    /// Optional model to use; None means use the provider's default.
    pub model: Option<String>,
    /// Working directory where the provider should execute.
    pub working_dir: PathBuf,
}

/// The outcome of a provider invocation.
///
/// Contains the exit code, standard output, standard error, optional usage
/// telemetry, and optional session identifier.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Process exit code from the provider.
    pub exit_code: i32,
    /// Standard output from the provider.
    pub stdout: String,
    /// Standard error from the provider.
    pub stderr: String,
    /// Optional usage telemetry from the provider.
    pub usage: Option<Usage>,
    /// Optional session identifier from the provider.
    pub session_id: Option<String>,
}

/// A provider that can execute invocations.
///
/// This trait defines the interface that all providers must implement.
/// Providers are interchangeable behind this trait, allowing the core
/// to work with different AI agents (Claude, Codex, etc.) uniformly.
pub trait Provider: Send + Sync {
    /// Returns the name of the provider.
    fn name(&self) -> &str;

    /// Returns the capabilities of the provider.
    fn capabilities(&self) -> Capabilities;

    /// Invokes the provider with the given invocation request.
    ///
    /// The optional bus parameter can be used to broadcast events during execution.
    /// The invocation includes the prompt, optional model selection, and working directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider cannot execute the invocation, such as
    /// authentication failure, network error, or other transient/configuration issues.
    fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome>;
}

/// Checks that configured and reported model IDs are consistent.
///
/// When both are present, they must match; a mismatch is rejected.
/// When one or both are missing, it is allowed and marked.
///
/// # Errors
///
/// Returns an error if both configured and reported models are present but differ.
pub fn check_model(configured: Option<&str>, reported: Option<&str>) -> Result<()> {
    match (configured, reported) {
        (Some(conf), Some(rep)) if conf != rep => Err(crate::Error::Provider {
            provider: "provider".to_string(),
            detail: format!("model mismatch: configured '{conf}' vs reported '{rep}'"),
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
/// Tests for model ID checking.
pub mod model_check {
    use super::*;

    #[test]
    fn both_none_is_ok() {
        assert!(check_model(None, None).is_ok());
    }

    #[test]
    fn configured_present_reported_none_is_ok() {
        assert!(check_model(Some("claude-opus"), None).is_ok());
    }

    #[test]
    fn configured_none_reported_present_is_ok() {
        assert!(check_model(None, Some("claude-opus")).is_ok());
    }

    #[test]
    fn both_present_and_equal_is_ok() {
        assert!(check_model(Some("claude-opus"), Some("claude-opus")).is_ok());
    }

    #[test]
    fn both_present_and_differ_is_err() {
        let result = check_model(Some("claude-opus"), Some("claude-sonnet"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("model mismatch"));
        assert!(msg.contains("claude-opus"));
        assert!(msg.contains("claude-sonnet"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trait_is_object_safe() {
        fn check_object_safe(_: &dyn Provider) {}
        let _ = check_object_safe;
    }

    #[test]
    fn capabilities_copy_and_eq() {
        let cap1 = Capabilities {
            structured_output: true,
            model_selection: false,
            usage_telemetry: true,
        };
        let cap2 = Capabilities {
            structured_output: true,
            model_selection: false,
            usage_telemetry: true,
        };
        assert_eq!(cap1, cap2);
    }

    #[test]
    fn invocation_creation() {
        let inv = Invocation {
            prompt: "Hello world".to_string(),
            model: Some("gpt-4".to_string()),
            working_dir: PathBuf::from("/tmp"),
        };
        assert_eq!(inv.prompt, "Hello world");
        assert_eq!(inv.model, Some("gpt-4".to_string()));
        assert_eq!(inv.working_dir, PathBuf::from("/tmp"));
    }

    #[test]
    fn outcome_creation() {
        let outcome = Outcome {
            exit_code: 0,
            stdout: "output".to_string(),
            stderr: String::new(),
            usage: None,
            session_id: Some("session-123".to_string()),
        };
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "output");
        assert_eq!(outcome.session_id, Some("session-123".to_string()));
    }
}

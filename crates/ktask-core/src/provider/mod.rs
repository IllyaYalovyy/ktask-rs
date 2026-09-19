//! Provider types for token and cost reporting and a stable capability interface.

use crate::{Bus, Result};
use std::path::PathBuf;

/// Token and cost usage types.
pub mod usage;

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

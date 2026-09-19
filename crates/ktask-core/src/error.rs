//! Error type and Result alias for ktask-core.

use std::path::PathBuf;
use thiserror::Error;

/// Error type for ktask-core operations.
///
/// Each variant carries enough context to name the operation and subject.
#[derive(Error, Debug)]
pub enum Error {
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Database error.
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// JSON serialization/deserialization error.
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Configuration error.
    #[error("Config error for key '{key}': {detail}")]
    Config {
        /// The configuration key that failed.
        key: String,
        /// Detailed error message.
        detail: String,
    },

    /// Git command execution error.
    #[error("Git error: {stderr}")]
    Git {
        /// The git command arguments that failed.
        args: Vec<String>,
        /// Standard error output from git.
        stderr: String,
    },

    /// Provider error.
    #[error("Provider '{provider}' error: {detail}")]
    Provider {
        /// The provider name.
        provider: String,
        /// Error detail.
        detail: String,
    },

    /// Gate execution error.
    #[error("Gate error: {detail}")]
    Gate {
        /// Gate kind (type marker for display).
        kind: String,
        /// Error detail.
        detail: String,
    },

    /// Policy violation.
    #[error("Policy error: {detail}")]
    Policy {
        /// Detailed error message.
        detail: String,
        /// Paths that violated the policy.
        paths: Vec<PathBuf>,
    },

    /// Invalid state transition.
    #[error("Invalid transition from {from} on {event}")]
    InvalidTransition {
        /// The state we were in.
        from: String,
        /// The event that triggered the invalid transition.
        event: String,
    },

    /// Item not found.
    #[error("Not found: {what}")]
    NotFound {
        /// What was not found.
        what: String,
    },

    /// Corrupted data.
    #[error("Corrupt data: {detail}")]
    Corrupt {
        /// Error detail.
        detail: String,
        /// Event sequence number if applicable.
        seq: Option<u64>,
    },
}

/// Result type for ktask-core operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "test");
        let err: Error = io_err.into();
        matches!(err, Error::Io(_));
    }

    #[test]
    fn invalid_transition_has_context() {
        let err = Error::InvalidTransition {
            from: "Running".to_string(),
            event: "Failed".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Running"));
        assert!(msg.contains("Failed"));
    }

    #[test]
    fn not_found_carries_what() {
        let err = Error::NotFound {
            what: "config.toml".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("config.toml"));
    }
}

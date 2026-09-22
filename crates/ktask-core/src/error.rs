//! The single error type for ktask-core, and its `Result` alias.
//!
//! Every variant names the operation and the subject that failed, so a
//! caller can decide what to do without re-deriving context from a bare
//! string. No variant carries a secret: git and provider failures carry
//! their process's stderr or a description, never credentials.

use std::path::PathBuf;

/// Everything that can go wrong inside ktask-core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem or other I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A SQLite operation on the journal failed.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// Serializing or deserializing an event payload failed.
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Formatting a timestamp failed.
    #[error("time formatting error: {0}")]
    Time(#[from] time::error::Format),

    /// A configuration value was missing or could not be parsed.
    #[error("config error for {key}: {detail}")]
    Config {
        /// The configuration key that failed to resolve.
        key: String,
        /// What went wrong with that key.
        detail: String,
    },

    /// A `git` subprocess invocation failed.
    #[error("git {args:?} failed: {stderr}")]
    Git {
        /// The argument vector passed to `git`.
        args: Vec<String>,
        /// The captured standard error.
        stderr: String,
    },

    /// A provider (agent backend) reported a failure.
    #[error("provider {provider} error: {detail}")]
    Provider {
        /// The provider's name.
        provider: String,
        /// What the provider reported.
        detail: String,
    },

    /// A quality gate failed.
    #[error("gate {kind} failed: {detail}")]
    Gate {
        /// Which gate failed, by name.
        kind: String,
        /// Why it failed.
        detail: String,
    },

    /// A policy was violated, such as a dirty working tree at verification.
    #[error("policy violation: {detail} ({paths:?})")]
    Policy {
        /// What the policy forbids.
        detail: String,
        /// Every offending path, not merely a count.
        paths: Vec<PathBuf>,
    },

    /// `commit_all` was asked to commit a worktree with no tracked changes
    /// staged, which would otherwise silently produce an empty commit.
    #[error("nothing to commit in {worktree}")]
    NothingToCommit {
        /// The worktree that had nothing staged.
        worktree: PathBuf,
    },

    /// An event could not be applied to the current state.
    #[error("invalid transition from {from} on event {event}")]
    InvalidTransition {
        /// The state the transition was attempted from.
        from: String,
        /// The event that could not be applied there.
        event: String,
    },

    /// The requested item does not exist.
    #[error("not found: {what}")]
    NotFound {
        /// What was being looked up.
        what: String,
    },

    /// The journal or a projection derived from it is corrupt.
    #[error("corrupt: {detail}")]
    Corrupt {
        /// What was found to be corrupt.
        detail: String,
    },
}

/// Convenience alias for `Result<T, Error>`, used throughout ktask-core.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_transition_names_state_and_event() {
        let err = Error::InvalidTransition {
            from: "Queued".to_string(),
            event: "VerifyPassed".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "invalid transition from Queued on event VerifyPassed"
        );
    }

    #[test]
    fn git_error_names_the_command_that_produced_it() {
        let err = Error::Git {
            args: vec!["push".to_string(), "origin".to_string(), "main".to_string()],
            stderr: "rejected: non-fast-forward".to_string(),
        };
        let message = err.to_string();
        assert!(message.contains("push"));
        assert!(message.contains("rejected: non-fast-forward"));
    }

    #[test]
    fn policy_error_names_every_offending_path() {
        let err = Error::Policy {
            detail: "dirty working tree".to_string(),
            paths: vec![PathBuf::from("src/lib.rs"), PathBuf::from("Cargo.lock")],
        };
        let message = err.to_string();
        assert!(message.contains("src/lib.rs"));
        assert!(message.contains("Cargo.lock"));
    }

    #[test]
    fn io_error_converts_via_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn database_error_converts_via_from() {
        let db_err = rusqlite::Error::InvalidQuery;
        let err: Error = db_err.into();
        assert!(matches!(err, Error::Database(_)));
    }

    #[test]
    fn serde_error_converts_via_from() {
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err: Error = serde_err.into();
        assert!(matches!(err, Error::Serde(_)));
    }

    #[test]
    fn not_found_names_the_subject() {
        let err = Error::NotFound {
            what: "task 7".to_string(),
        };
        assert_eq!(err.to_string(), "not found: task 7");
    }
}

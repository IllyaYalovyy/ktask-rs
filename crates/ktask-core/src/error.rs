//! The one error type every fallible operation in the core returns.
//!
//! A supervisor that panics loses the run it was supervising, so nothing here
//! is a placeholder: each variant carries the operation's subject — the config
//! key, the git argument vector, the offending paths, the state and event of an
//! illegal transition — so a failure can be reported without the caller
//! reconstructing what it was doing.
//!
//! A message never contains a credential. The variants that carry text from
//! outside this process (`Git::stderr`, `Provider::detail`) hold what the
//! caller handed them, and a caller passes text that secret redaction has
//! already been run over; the formats below add only the fixed labels that name
//! the subsystem.

use std::path::PathBuf;

/// Every way a core operation can fail, and enough context to say so.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The filesystem, a pipe or a socket refused the operation. The wrapped
    /// error names the path it touched and the OS reason.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// SQLite refused a statement, or a journal read returned something the
    /// schema cannot describe.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// A payload that must round-trip through JSON did not.
    #[error("json error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A configuration key was absent, malformed or carried an unknown value.
    /// `key` is the name of the key, never its value.
    #[error("config error `{key}`: {detail}")]
    Config {
        /// The configuration key at issue, as it is written in the file.
        key: String,
        /// What about the key is wrong.
        detail: String,
    },

    /// The `git` subprocess exited non-zero. `args` is the argument vector that
    /// ran, so the failure names the command rather than a paraphrase of it.
    #[error("git `{}` failed: {stderr}", args.join(" "))]
    Git {
        /// The arguments `git` was run with, without the program name.
        args: Vec<String>,
        /// The last useful line(s) of its standard error.
        stderr: String,
    },

    /// A coding-agent CLI could not be started, or refused the work.
    #[error("provider `{provider}` failed: {detail}")]
    Provider {
        /// The adapter that was asked to run: `dummy`, `claude`, `codex`.
        provider: String,
        /// What it reported, or why it could not be reached.
        detail: String,
    },

    /// A mechanical gate ran and did not pass. The gate is identified by kind,
    /// because the kind is what a human is being asked to acknowledge.
    #[error("gate `{kind}` failed: {detail}")]
    Gate {
        /// The kind of gate that failed.
        kind: String,
        /// Its exit status, timeout, or the check that refused.
        detail: String,
    },

    /// A repository or state-directory rule was broken. Every offending path is
    /// listed: naming one of them would send a human to look at the wrong file.
    #[error("policy violation: {detail} (offending paths: {})", path_list(paths))]
    Policy {
        /// The rule that was broken.
        detail: String,
        /// Each path that broke it.
        paths: Vec<PathBuf>,
    },

    /// The state machine was asked for a transition its current state forbids.
    /// Both names are the ones an operator reads in the TUI.
    #[error("illegal transition from `{from}` on event `{event}`")]
    InvalidTransition {
        /// The state that was asked to move.
        from: String,
        /// The event it refused.
        event: String,
    },

    /// The subject of the operation is not there — a task, a project, a run.
    #[error("not found: {what}")]
    NotFound {
        /// What was looked for, identified the way the interface identifies it.
        what: String,
    },

    /// Durable data was read and could not be trusted. `seq` locates the record
    /// in the journal when the reader got far enough to know it.
    #[error("corrupt data{location}: {detail}", location = seq_location(*seq))]
    Corrupt {
        /// What could not be read.
        detail: String,
        /// The journal sequence of the record, if one was reached.
        seq: Option<u64>,
    },
}

/// Where a corrupt record was read, phrased so an absent location is silent
/// rather than a guess at one.
fn seq_location(seq: Option<u64>) -> String {
    match seq {
        Some(seq) => format!(" at seq {seq}"),
        None => String::new(),
    }
}

/// The offending paths, written the way the filesystem named them.
fn path_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The result every core operation returns: `T`, or [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, Result};
    use std::error::Error as _;
    use std::path::PathBuf;

    #[test]
    fn io_failure_converts_and_is_labelled_io() {
        let error: Error = std::io::Error::other("no such file or directory").into();
        assert!(matches!(error, Error::Io(_)));
        assert_eq!(error.to_string(), "io error: no such file or directory");
    }

    #[test]
    fn database_failure_converts_and_is_labelled_database() {
        let error: Error = rusqlite::Error::InvalidParameterName("task_id".to_owned()).into();
        assert!(matches!(error, Error::Database(_)));
        let message = error.to_string();
        assert_eq!(
            message,
            format!(
                "database error: {}",
                rusqlite::Error::InvalidParameterName("task_id".to_owned())
            )
        );
    }

    #[test]
    fn json_failure_converts_and_is_labelled_json() {
        let malformed = serde_json::from_str::<serde_json::Value>("{ kind: }")
            .expect_err("the payload is not valid JSON");
        let reported = malformed.to_string();
        let error: Error = malformed.into();
        assert!(matches!(error, Error::Serde(_)));
        assert_eq!(error.to_string(), format!("json error: {reported}"));
        assert!(reported.contains("line 1 column"), "{reported}");
    }

    #[test]
    fn config_error_names_the_key_and_what_is_wrong_with_it() {
        let error = Error::Config {
            key: "provider".to_owned(),
            detail: "unknown value `zed`".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "config error `provider`: unknown value `zed`"
        );
    }

    #[test]
    fn git_error_names_the_command_that_ran_and_its_stderr() {
        let args = ["push".to_owned(), "origin".to_owned(), "main".to_owned()].to_vec();
        let error = Error::Git {
            args,
            stderr: "rejected: non-fast-forward".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "git `push origin main` failed: rejected: non-fast-forward"
        );
    }

    #[test]
    fn provider_error_names_the_provider_and_what_it_reported() {
        let error = Error::Provider {
            provider: "codex".to_owned(),
            detail: "exited 1 before emitting any event".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "provider `codex` failed: exited 1 before emitting any event"
        );
    }

    #[test]
    fn gate_error_names_the_gate_and_why_it_failed() {
        let error = Error::Gate {
            kind: "verify".to_owned(),
            detail: "timed out after 1800s".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "gate `verify` failed: timed out after 1800s"
        );
    }

    #[test]
    fn policy_error_lists_every_offending_path_not_a_count_of_them() {
        let paths = vec![
            PathBuf::from(".ktask/config.toml"),
            PathBuf::from(".ktask/queue/current-task.md"),
        ];
        let error = Error::Policy {
            detail: "operational state touched".to_owned(),
            paths,
        };
        assert_eq!(
            error.to_string(),
            "policy violation: operational state touched (offending paths: \
             .ktask/config.toml, .ktask/queue/current-task.md)"
        );
    }

    #[test]
    fn invalid_transition_names_the_state_and_the_event_rejected() {
        let error = Error::InvalidTransition {
            from: "Done".to_owned(),
            event: "AttemptStarted".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "illegal transition from `Done` on event `AttemptStarted`"
        );
    }

    #[test]
    fn not_found_names_the_subject_that_is_missing() {
        let error = Error::NotFound {
            what: "task 12".to_owned(),
        };
        assert_eq!(error.to_string(), "not found: task 12");
    }

    #[test]
    fn corrupt_names_the_sequence_it_was_read_at() {
        let error = Error::Corrupt {
            detail: "payload is not a known event kind".to_owned(),
            seq: Some(12),
        };
        assert_eq!(
            error.to_string(),
            "corrupt data at seq 12: payload is not a known event kind"
        );
    }

    #[test]
    fn corrupt_without_a_sequence_omits_the_location_rather_than_inventing_one() {
        let error = Error::Corrupt {
            detail: "header is missing the schema version".to_owned(),
            seq: None,
        };
        assert_eq!(
            error.to_string(),
            "corrupt data: header is missing the schema version"
        );
    }

    #[test]
    fn wrapped_failures_expose_what_caught_them_as_the_source() {
        let io: Error = std::io::Error::other("disk full").into();
        assert_eq!(
            io.source().map(ToString::to_string),
            Some("disk full".to_owned())
        );

        let database: Error = rusqlite::Error::InvalidParameterName("seq".to_owned()).into();
        assert!(database.source().is_some());

        let malformed = serde_json::from_str::<serde_json::Value>("nul")
            .expect_err("the payload is not valid JSON");
        let json: Error = malformed.into();
        assert!(json.source().is_some());
    }

    #[test]
    fn a_variant_with_no_wrapped_error_reports_no_source() {
        let error = Error::NotFound {
            what: "project".to_owned(),
        };
        assert!(error.source().is_none());
    }

    #[test]
    fn the_result_alias_never_names_its_error_type_at_the_call_site() {
        fn load() -> Result<u32> {
            Err(Error::NotFound {
                what: "task 3".to_owned(),
            })
        }
        let err = load().expect_err("an unqueued task cannot be loaded");
        assert_eq!(err.to_string(), "not found: task 3");
    }
}

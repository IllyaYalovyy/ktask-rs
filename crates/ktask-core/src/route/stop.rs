//! Environment faults: mechanical checks against a failed step's exit code and reason, so a
//! machine problem is never handed to a model and never costs the task an attempt. Each one
//! stops the run with its own what, why and exact fix.

/// A failure the tool recognises on its own, with its own fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    /// The program a step needed was not on `PATH` at all: the shell that tried to run it
    /// exited `127`, the POSIX convention for "command not found".
    ProgramNotFound,
    /// The machine's disk has no space left.
    DiskFull,
    /// The machine has no file descriptors left to hand out.
    FileSlotsFull,
    /// The commit step found changes to commit, but git has no identity configured to commit
    /// them under.
    GitIdentityMissing,
    /// The project's tracked remote could not be reached.
    RemoteUnreachable,
    /// Claude Code needs its operator to authenticate again.
    ClaudeAuthentication,
    /// Claude Code rejected one of its settings files.
    ClaudeConfiguration,
    /// Codex needs the operator to authenticate.
    CodexAuthentication,
}

/// The phrase an operating system error names a full disk with, verbatim, on Linux.
const DISK_FULL_PHRASE: &str = "No space left on device";
/// The phrase an operating system error names an exhausted file-descriptor table with,
/// verbatim, on Linux.
const FILE_SLOTS_FULL_PHRASE: &str = "Too many open files";
/// The phrase [`crate::steps::commit::IDENTITY_NOT_CONFIGURED`] always starts with.
const GIT_IDENTITY_MISSING_PHRASE: &str = "git identity is not configured";
/// Phrases, lower-cased, any of which git itself is known to say when a remote cannot be
/// reached — compared against `reason`, lower-cased the same way.
const REMOTE_UNREACHABLE_PHRASES: [&str; 6] = [
    "could not read from remote repository",
    "does not appear to be a git repository",
    "could not resolve host",
    "unable to access",
    "connection refused",
    "connection timed out",
];
const CLAUDE_AUTHENTICATION_PHRASES: [&str; 2] = ["invalid api key", "not logged in"];
const CLAUDE_CONFIGURATION_PHRASE: &str = "invalid settings";
const CODEX_AUTHENTICATION_PHRASES: [&str; 2] =
    ["missing bearer or basic authentication", "401 unauthorized"];

impl StopCause {
    /// Every cause, in the order [`Self::classify`] checks them.
    pub(crate) const ALL: [Self; 8] = [
        Self::ProgramNotFound,
        Self::DiskFull,
        Self::FileSlotsFull,
        Self::GitIdentityMissing,
        Self::RemoteUnreachable,
        Self::ClaudeAuthentication,
        Self::ClaudeConfiguration,
        Self::CodexAuthentication,
    ];

    /// The name this cause is kept under in the journal.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::ProgramNotFound => "program-not-found",
            Self::DiskFull => "disk-full",
            Self::FileSlotsFull => "file-slots-full",
            Self::GitIdentityMissing => "git-identity-missing",
            Self::RemoteUnreachable => "remote-unreachable",
            Self::ClaudeAuthentication => "claude-authentication",
            Self::ClaudeConfiguration => "claude-configuration",
            Self::CodexAuthentication => "codex-authentication",
        }
    }

    /// The cause kept under `token` in the journal.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|cause| cause.token() == token)
    }

    /// What matches `exit_code` and `reason` against every known cause, in the order checked:
    /// `exit_code` first, since it needs no `reason` at all, then `reason` itself against each
    /// remaining cause's own phrase. `None` when nothing matches — the failure goes to the
    /// resolver as before.
    pub(crate) fn classify(exit_code: Option<i32>, reason: Option<&str>) -> Option<Self> {
        if exit_code == Some(127) {
            return Some(Self::ProgramNotFound);
        }
        let reason = reason?;
        if reason.contains(DISK_FULL_PHRASE) {
            return Some(Self::DiskFull);
        }
        if reason.contains(FILE_SLOTS_FULL_PHRASE) {
            return Some(Self::FileSlotsFull);
        }
        if reason.contains(GIT_IDENTITY_MISSING_PHRASE) {
            return Some(Self::GitIdentityMissing);
        }
        let lower = reason.to_lowercase();
        if CLAUDE_AUTHENTICATION_PHRASES
            .iter()
            .any(|phrase| lower.contains(phrase))
        {
            return Some(Self::ClaudeAuthentication);
        }
        if lower.contains(CLAUDE_CONFIGURATION_PHRASE) {
            return Some(Self::ClaudeConfiguration);
        }
        if CODEX_AUTHENTICATION_PHRASES
            .iter()
            .any(|phrase| lower.contains(phrase))
        {
            return Some(Self::CodexAuthentication);
        }
        REMOTE_UNREACHABLE_PHRASES
            .iter()
            .any(|phrase| lower.contains(phrase))
            .then_some(Self::RemoteUnreachable)
    }

    /// What, why and the exact command that fixes it, in one line even when `reason` itself
    /// has several (git's own messages routinely do) — already complete, for
    /// [`Self::GitIdentityMissing`], which names its own exact `git config` commands.
    pub(crate) fn message(self, reason: &str) -> String {
        let reason = reason.replace('\n', " / ");
        match self {
            Self::ProgramNotFound => format!(
                "a program this step needed could not be found on PATH: {reason}; install it, \
                 or add it to PATH, then run again"
            ),
            Self::DiskFull => {
                format!("the disk is full: {reason}; free up disk space, then run again")
            }
            Self::FileSlotsFull => format!(
                "too many files are open on this machine: {reason}; close other processes, or \
                 raise the open-file limit, then run again"
            ),
            Self::GitIdentityMissing => reason,
            Self::RemoteUnreachable => {
                format!("{reason}; make the remote reachable, then run again")
            }
            Self::ClaudeAuthentication => format!(
                "Claude Code could not authenticate: {reason}; run `claude /login`, then run again"
            ),
            Self::ClaudeConfiguration => format!(
                "Claude Code has invalid settings: {reason}; fix the named Claude Code settings file, then run again"
            ),
            Self::CodexAuthentication => {
                format!("Codex could not authenticate: {reason}; run `codex login`, then run again")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_127_is_a_program_not_found_whatever_the_reason() {
        assert_eq!(
            StopCause::classify(Some(127), None),
            Some(StopCause::ProgramNotFound)
        );
        assert_eq!(
            StopCause::classify(Some(127), Some("anything")),
            Some(StopCause::ProgramNotFound)
        );
    }

    #[test]
    fn a_disk_full_error_is_recognised_in_the_reason() {
        assert_eq!(
            StopCause::classify(
                None,
                Some(
                    "the provider could not run: cannot run bash: No space left on device (os error 28)"
                )
            ),
            Some(StopCause::DiskFull)
        );
    }

    #[test]
    fn a_file_slots_full_error_is_recognised_in_the_reason() {
        assert_eq!(
            StopCause::classify(
                None,
                Some(
                    "the provider could not run: cannot run bash: Too many open files (os error 24)"
                )
            ),
            Some(StopCause::FileSlotsFull)
        );
    }

    #[test]
    fn a_missing_git_identity_is_recognised_in_the_reason() {
        assert_eq!(
            StopCause::classify(
                None,
                Some(
                    "git identity is not configured: set it with `git config user.name \
                     \"Your Name\"` and `git config user.email you@example.com`, then run again"
                )
            ),
            Some(StopCause::GitIdentityMissing)
        );
    }

    #[test]
    fn an_unreachable_remote_is_recognised_case_insensitively_in_the_reason() {
        assert_eq!(
            StopCause::classify(
                None,
                Some(
                    "`git push origin HEAD:refs/heads/main` exited with code 128: fatal: \
                     Could not read from remote repository."
                )
            ),
            Some(StopCause::RemoteUnreachable)
        );
    }

    #[test]
    fn a_remote_that_is_not_even_a_git_repository_is_recognised_as_unreachable_too() {
        assert_eq!(
            StopCause::classify(
                None,
                Some(
                    "`git push origin HEAD:refs/heads/main` exited with code 128: fatal: \
                     '/no/such/path' does not appear to be a git repository"
                )
            ),
            Some(StopCause::RemoteUnreachable)
        );
    }

    #[test]
    fn nothing_known_matches_an_ordinary_failure() {
        assert_eq!(
            StopCause::classify(Some(1), Some("the tests did not pass")),
            None
        );
        assert_eq!(StopCause::classify(None, None), None);
    }

    #[test]
    fn claude_authentication_and_configuration_errors_are_known_causes() {
        assert_eq!(
            StopCause::classify(None, Some("Invalid API key · Please run /login")),
            Some(StopCause::ClaudeAuthentication)
        );
        assert_eq!(
            StopCause::classify(
                None,
                Some("Error: Invalid settings at ~/.claude/settings.json")
            ),
            Some(StopCause::ClaudeConfiguration)
        );
    }

    #[test]
    fn the_git_identity_message_is_the_reason_verbatim() {
        let reason = "git identity is not configured: do the thing";
        assert_eq!(StopCause::GitIdentityMissing.message(reason), reason);
    }

    #[test]
    fn every_other_message_names_what_why_and_a_fix_on_one_line() {
        for (cause, reason) in [
            (StopCause::ProgramNotFound, "exited 127"),
            (StopCause::DiskFull, "No space left on device"),
            (StopCause::FileSlotsFull, "Too many open files"),
            (StopCause::RemoteUnreachable, "could not read from remote"),
        ] {
            let message = cause.message(reason);
            assert_eq!(message.lines().count(), 1, "{message:?}");
            assert!(message.contains(reason), "{message}");
            assert!(message.contains("run again"), "{message}");
            assert_eq!(
                repeated_phrase(&message.replacen(reason, "", 1)),
                None,
                "{message}"
            );
        }
    }

    #[test]
    fn a_message_that_says_the_same_thing_twice_is_caught() {
        assert_eq!(
            repeated_phrase(
                "Codex lost its transport repeatedly: Codex transport failed; its transport repeatedly fails"
            ),
            Some("its transport".to_owned())
        );
        assert_eq!(repeated_phrase("the disk is full: x; free up space"), None);
    }

    /// The first two-word phrase that occurs twice in `message`, compared case-insensitively
    /// and ignoring punctuation.
    fn repeated_phrase(message: &str) -> Option<String> {
        let words: Vec<String> = message
            .split_whitespace()
            .map(|word| {
                word.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .collect();
        (0..words.len().saturating_sub(1)).find_map(|start| {
            let pair = &words[start..start + 2];
            (start + 2..words.len().saturating_sub(1))
                .any(|later| words[later..later + 2] == *pair)
                .then(|| pair.join(" "))
        })
    }
}

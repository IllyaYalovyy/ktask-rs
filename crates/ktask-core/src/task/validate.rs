//! Why a task draft may be refused, and the rules that decide it: a blank title or
//! criterion, a control character where one is not allowed, or a link that is neither a
//! `github:owner/repo#NUMBER` reference nor an `http(s)` URL.

use std::error::Error;
use std::fmt;

use crate::{AppendError, JournalError};

use super::{TaskDraft, TaskId};

/// Why a task was not added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    /// The title is empty or only whitespace.
    EmptyTitle,
    /// The title holds a character it may not: a control character.
    ControlCharacterInTitle(char),
    /// There is no acceptance criterion.
    NoCriteria,
    /// An acceptance criterion is empty or only whitespace.
    EmptyCriterion,
    /// An acceptance criterion holds a character it may not: a control character.
    ControlCharacterInCriterion(char),
    /// A link is neither a `github:owner/repo#NUMBER` reference nor an `http(s)` URL.
    MalformedLink(String),
    /// A link holds a character it may not: a control character.
    ControlCharacterInLink(char),
    /// The body holds a character it may not: a control character other than a newline or a
    /// tab.
    ControlCharacterInBody(char),
    /// The task the new one was to be placed next to does not exist.
    UnknownTask(TaskId),
    /// The task the new one was to be placed next to was cancelled.
    CancelledTask(TaskId),
    /// The journal could not be used.
    Journal(JournalError),
}

/// `c` in a readable form that never prints the character itself: `\n`, `\t`, `\x1b`, or
/// `\u{...}` for anything wider than a byte.
fn readable_control_char(c: char) -> String {
    match c {
        '\n' => "\\n".to_owned(),
        '\t' => "\\t".to_owned(),
        '\r' => "\\r".to_owned(),
        c if u32::from(c) < 0x100 => format!("\\x{:02x}", u32::from(c)),
        c => format!("\\u{{{:x}}}", u32::from(c)),
    }
}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTitle => f.write_str("the title is empty: a task needs a title"),
            Self::ControlCharacterInTitle(c) => write!(
                f,
                "the title contains a control character: {}",
                readable_control_char(*c)
            ),
            Self::NoCriteria => f.write_str("a task needs at least one acceptance criterion"),
            Self::EmptyCriterion => f.write_str("an acceptance criterion is empty"),
            Self::ControlCharacterInCriterion(c) => write!(
                f,
                "an acceptance criterion contains a control character: {}",
                readable_control_char(*c)
            ),
            Self::MalformedLink(link) => write!(
                f,
                "malformed link {link:?}: expected github:owner/repo#NUMBER or an http(s) URL"
            ),
            Self::ControlCharacterInLink(c) => write!(
                f,
                "a link contains a control character: {}",
                readable_control_char(*c)
            ),
            Self::ControlCharacterInBody(c) => write!(
                f,
                "the body contains a control character: {}",
                readable_control_char(*c)
            ),
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::CancelledTask(id) => write!(f, "task {id} is cancelled"),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for AddError {}

impl From<AppendError> for AddError {
    fn from(error: AppendError) -> Self {
        match error {
            AppendError::UnknownTask(id) => Self::UnknownTask(id),
            AppendError::CancelledTask(id) => Self::CancelledTask(id),
        }
    }
}

impl From<JournalError> for AddError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Whether `link` is a `github:owner/repo#NUMBER` reference or an `http(s)` URL.
pub(super) fn is_link(link: &str) -> bool {
    let name = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if let Some(reference) = link.strip_prefix("github:") {
        let Some((repository, number)) = reference.split_once('#') else {
            return false;
        };
        let Some((owner, repo)) = repository.split_once('/') else {
            return false;
        };
        return name(owner)
            && name(repo)
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit());
    }
    let rest = link
        .strip_prefix("https://")
        .or_else(|| link.strip_prefix("http://"));
    rest.is_some_and(|rest| {
        !rest.is_empty() && !rest.starts_with('/') && !rest.chars().any(char::is_whitespace)
    })
}

/// The first character of `text` that is a control character and not one of `allowed`.
fn control_char(text: &str, allowed: &[char]) -> Option<char> {
    text.chars()
        .find(|c| c.is_control() && !allowed.contains(c))
}

/// Every rule `draft` breaks, in the order of its fields; empty when it may be added.
pub(crate) fn draft_problems(draft: &TaskDraft) -> Vec<AddError> {
    let mut problems = Vec::new();
    if draft.title.trim().is_empty() {
        problems.push(AddError::EmptyTitle);
    }
    if let Some(c) = control_char(&draft.title, &[]) {
        problems.push(AddError::ControlCharacterInTitle(c));
    }
    if let Some(c) = control_char(&draft.body, &['\n', '\t']) {
        problems.push(AddError::ControlCharacterInBody(c));
    }
    if draft.criteria.is_empty() {
        problems.push(AddError::NoCriteria);
    }
    if draft.criteria.iter().any(|c| c.trim().is_empty()) {
        problems.push(AddError::EmptyCriterion);
    }
    problems.extend(draft.criteria.iter().filter_map(|criterion| {
        control_char(criterion, &[]).map(AddError::ControlCharacterInCriterion)
    }));
    for link in &draft.links {
        if let Some(c) = control_char(link, &[]) {
            problems.push(AddError::ControlCharacterInLink(c));
        } else if !is_link(link) {
            problems.push(AddError::MalformedLink(link.clone()));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_character_message_names_the_field_and_a_readable_form_of_the_character() {
        assert_eq!(
            AddError::ControlCharacterInTitle('\n').to_string(),
            "the title contains a control character: \\n"
        );
        assert_eq!(
            AddError::ControlCharacterInCriterion('\t').to_string(),
            "an acceptance criterion contains a control character: \\t"
        );
        assert_eq!(
            AddError::ControlCharacterInLink('\x1b').to_string(),
            "a link contains a control character: \\x1b"
        );
        assert_eq!(
            AddError::ControlCharacterInBody('\x07').to_string(),
            "the body contains a control character: \\x07"
        );
    }

    #[test]
    fn github_references_and_http_urls_are_links() {
        for link in [
            "github:owner/repo#1",
            "github:my-org/my_repo.rs#123",
            "http://example.com",
            "https://example.com/a/b?c=d#e",
        ] {
            assert!(is_link(link), "{link}");
        }
    }

    #[test]
    fn anything_else_is_not_a_link() {
        for link in [
            "",
            "github:",
            "github:owner/repo",
            "github:owner/repo#",
            "github:owner/repo#x1",
            "github:owner#1",
            "github:/repo#1",
            "github:owner/repo/more#1",
            "github:o wner/repo#1",
            "gitlab:owner/repo#1",
            "ftp://example.com",
            "https://",
            "https:///path",
            "https://exa mple.com",
            "example.com",
        ] {
            assert!(!is_link(link), "{link}");
        }
    }
}

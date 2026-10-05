//! Selecting and terminal-safe rendering of retained provider output.

use std::error::Error;
use std::fmt;
use std::time::SystemTime;

use crate::{StatusEntry, TaskId};

mod claude;
mod codex;

/// Port: when an attempt's append-only provider output was last written.
///
/// An absent value means the provider has not written anything yet, or its retained output is
/// no longer available. Reading this is deliberately best-effort: an unavailable transcript
/// must not make status unavailable too.
pub trait AttemptOutput {
    /// The most recent write to `task`'s `attempt` output, when there is one.
    fn last_output_at(&self, task: TaskId, attempt: u32) -> Option<SystemTime>;
}

/// An output source with no retained output. This keeps callers that only need historical
/// status independent from the filesystem-backed live-output adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAttemptOutput;

impl AttemptOutput for NoAttemptOutput {
    fn last_output_at(&self, _task: TaskId, _attempt: u32) -> Option<SystemTime> {
        None
    }
}

/// Why an attempt's retained output cannot be selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputError {
    /// The task has never been attempted.
    NoAttempts(TaskId),
    /// The task was attempted, but not with the requested number.
    UnknownAttempt(TaskId, u32),
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAttempts(task) => write!(f, "task {task} has no attempts"),
            Self::UnknownAttempt(task, attempt) => {
                write!(f, "task {task} has no attempt {attempt}")
            }
        }
    }
}

impl Error for OutputError {}

/// Selects an attempt from the status view. With no explicit number the latest attempt wins.
/// This is shared by the CLI and TUI so opening either view names precisely the same output.
///
/// # Errors
///
/// Returns [`OutputError`] when the task has no retained attempt or its requested number does
/// not exist.
pub fn select_attempt(
    entries: &[StatusEntry],
    task: TaskId,
    requested: Option<u32>,
) -> Result<u32, OutputError> {
    let Some(entry) = entries.iter().find(|entry| entry.task == task) else {
        return Err(OutputError::NoAttempts(task));
    };
    let attempt = requested.unwrap_or(entry.attempt.number);
    if attempt == entry.attempt.number || entry.history.iter().any(|older| older.number == attempt)
    {
        Ok(attempt)
    } else {
        Err(OutputError::UnknownAttempt(task, attempt))
    }
}

/// The last agent provider that wrote retained output for an attempt. A completed command step
/// may be current after it, so its provider alone is not enough to identify the stream format.
#[must_use]
pub fn attempt_provider(entries: &[StatusEntry], task: TaskId, number: u32) -> Option<&str> {
    let attempt = entries
        .iter()
        .find(|entry| entry.task == task)
        .and_then(|entry| {
            if entry.attempt.number == number {
                Some(&entry.attempt)
            } else {
                entry.history.iter().find(|older| older.number == number)
            }
        })?;
    attempt.provider.as_deref().or_else(|| {
        attempt
            .steps
            .iter()
            .rev()
            .find_map(|step| step.provider.as_deref())
    })
}

/// Makes output safe to draw: control bytes are visible, carriage returns become newlines,
/// invalid UTF-8 is replaced, and a hostile line is wrapped before it dominates a frame.
#[must_use]
pub fn sanitize_output(bytes: &[u8]) -> String {
    const MAX_LINE: usize = 240;
    let mut safe = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'\r' => safe.push(b'\n'),
            b'\t' => safe.extend_from_slice(b"    "),
            b'\n' | 0x20..=0x7e | 0x80..=0xff => safe.push(byte),
            other => safe.extend_from_slice(format!("\\x{other:02x}").as_bytes()),
        }
    }
    let text = String::from_utf8_lossy(&safe);
    let mut rendered = String::with_capacity(text.len());
    let mut width = 0;
    for character in text.chars() {
        if character == '\n' {
            rendered.push(character);
            width = 0;
        } else {
            if width == MAX_LINE {
                rendered.push('\n');
                width = 0;
            }
            rendered.push(character);
            width += 1;
        }
    }
    rendered
}

/// Renders retained provider bytes into the words an operator reads. Plain providers retain
/// their own text; structured streams become one short line per visible event.
#[must_use]
pub fn render_provider_output(parser: crate::ProviderParser, bytes: &[u8]) -> String {
    match parser {
        crate::ProviderParser::Plain => String::from_utf8_lossy(bytes).into_owned(),
        crate::ProviderParser::ClaudeStreamJson => claude::render(bytes),
        crate::ProviderParser::CodexJsonl => codex::render(bytes),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{AttemptLine, AttemptOutcome, StatusEntry, TaskStatus};

    use super::{
        OutputError, attempt_provider, render_provider_output, sanitize_output, select_attempt,
    };

    #[test]
    fn controls_invalid_utf8_and_long_lines_are_safe_and_bounded() {
        let mut bytes = b"before\x1b[2J\ra\xff\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 10_000));
        let output = sanitize_output(&bytes);
        assert!(output.contains("before\\x1b[2J\na�"), "{output:?}");
        assert!(output.lines().all(|line| line.chars().count() <= 240));
    }

    fn attempt(number: u32) -> AttemptLine {
        AttemptLine {
            number,
            step: String::new(),
            provider: None,
            model: None,
            session: None,
            time_spent: Duration::ZERO,
            outcome: AttemptOutcome::Passed,
            reason: None,
            waiting_for: None,
            limit_wait: None,
            limit_warning: None,
            usage: crate::Usage::default(),
            output_activity: None,
            steps: vec![],
        }
    }

    #[test]
    fn latest_and_explicit_attempts_are_selected_from_one_place() {
        let entries = vec![StatusEntry {
            task: crate::TaskId(8),
            title: String::new(),
            status: TaskStatus::Done,
            attempt: attempt(3),
            history: vec![attempt(1), attempt(2)],
            done_by_user: None,
        }];
        assert_eq!(select_attempt(&entries, crate::TaskId(8), None), Ok(3));
        assert_eq!(select_attempt(&entries, crate::TaskId(8), Some(1)), Ok(1));
        assert_eq!(
            select_attempt(&entries, crate::TaskId(8), Some(4)),
            Err(OutputError::UnknownAttempt(crate::TaskId(8), 4))
        );
    }

    #[test]
    fn plain_provider_output_is_left_as_the_provider_wrote_it() {
        assert_eq!(
            render_provider_output(crate::ProviderParser::Plain, b"echoed\ntext"),
            "echoed\ntext"
        );
    }

    #[test]
    fn output_parser_uses_the_last_agent_step_after_a_command_step_finishes() {
        let mut finished = attempt(1);
        finished.steps = vec![crate::StepLine {
            step: "implementation".to_owned(),
            provider: Some("claude".to_owned()),
            model: None,
            session: None,
            time_spent: Duration::ZERO,
            outcome: AttemptOutcome::Passed,
            reason: None,
            waiting_for: None,
            limit_wait: None,
            limit_warning: None,
            usage: crate::Usage::default(),
        }];
        finished.provider = None;
        let entries = vec![StatusEntry {
            task: crate::TaskId(8),
            title: String::new(),
            status: TaskStatus::Done,
            attempt: finished,
            history: vec![],
            done_by_user: None,
        }];
        assert_eq!(
            attempt_provider(&entries, crate::TaskId(8), 1),
            Some("claude")
        );
    }
}

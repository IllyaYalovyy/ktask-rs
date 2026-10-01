//! [`Refusal`]: why a key was refused at once, without asking or opening anything, worded
//! identically to the CLI command it mirrors.

use ktask_core::{AnswerError, AppendError, CancelError, RetryError, TaskId, TaskStatus};

/// Why a key was refused at once, without asking or opening anything: the same reason, in the
/// same words, that `ktask-rs remove`, `ktask-rs add`, `ktask-rs retry` or `ktask-rs answer`
/// gives for the same situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `d` on the selected task, which is running: `ktask-rs remove` would refuse it too.
    Running(TaskId),
    /// `d` on the selected task, which is cancelled already: `ktask-rs remove` would refuse
    /// it too.
    AlreadyCancelled(TaskId),
    /// `o` or `O` next to the selected task, which is cancelled: `ktask-rs add` would refuse
    /// a task placed next to it too.
    NextToCancelled(TaskId),
    /// `t` on the selected task, whose status is not `failed`, `failed-unknown` or `blocked`:
    /// `ktask-rs retry` would refuse it too, naming the same status.
    NotRetryable(TaskId, TaskStatus),
    /// `A` on the selected task, whose status is not `blocked`: `ktask-rs answer` would refuse
    /// it too, naming the same status.
    NotBlocked(TaskId, TaskStatus),
}

impl Refusal {
    /// The message shown for this refusal, word for word what the CLI command it mirrors
    /// would print.
    pub(crate) fn message(self) -> String {
        match self {
            Self::Running(id) => CancelError::Running(id).to_string(),
            Self::AlreadyCancelled(id) => CancelError::AlreadyCancelled(id).to_string(),
            Self::NextToCancelled(id) => AppendError::CancelledTask(id).to_string(),
            Self::NotRetryable(id, status) => RetryError::NotRetryable { id, status }.to_string(),
            Self::NotBlocked(id, status) => AnswerError::NotBlocked { id, status }.to_string(),
        }
    }
}

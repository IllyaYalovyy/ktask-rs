//! The journal: where a project's events and the tasks projected from them are kept.

use std::error::Error;
use std::fmt;
use std::time::SystemTime;

use crate::{Task, TaskDraft};

/// Why the journal could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalError {
    message: String,
}

impl JournalError {
    /// An error described by `message`, which names what failed and why.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for JournalError {}

/// Port: one project's journal. Every change is an event appended to it, and the tasks are
/// updated from that event in the same transaction.
pub trait Journal {
    /// Appends a task-added event for `draft`, which the caller has validated, and puts the
    /// task at the end of the queue with the next number, in one transaction. Numbers start
    /// at 1 and are never reused. Returns the task as stored.
    ///
    /// # Errors
    ///
    /// Fails, recording nothing, when the journal cannot be written.
    fn append_task(&self, draft: &TaskDraft, at: SystemTime) -> Result<Task, JournalError>;

    /// Every task, in queue order, with positions counting from 1.
    ///
    /// # Errors
    ///
    /// Fails when the journal cannot be read.
    fn tasks(&self) -> Result<Vec<Task>, JournalError>;
}

//! The task as the queue holds it in memory: what was asked, and how it is
//! proved.
//!
//! A task is the text an operator authored, imported once and kept verbatim.
//! `body` is the block as written so the original document can be shown again
//! on the Task detail screen; the four named fields are the sections
//! `.ktask/README.md` requires of every task, held separately because the
//! queue stores them as separate columns and the UI reads them separately.
//!
//! Nothing here decides whether a task is finished. [`TaskStatus`] says what the
//! supervisor concluded, and that conclusion comes from the journal, never from
//! this struct — an agent's own claim is not evidence (VISION.md §2).
//!
//! The state machine that drives a task through preflight, attempts and gates is
//! `TaskState` in `state.rs`, a different and much richer type. This is the
//! queue entry; that is the run.

use crate::ids::TaskId;

/// What the queue holds a task for, at the level the list screen shows.
///
/// The five states an operator acts on. The intermediate states of a run —
/// preflight, an attempt in progress, publishing — belong to
/// `TaskState` and are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Queued and not yet run.
    Pending,
    /// Verified and published.
    Done,
    /// Terminally failed; the run stopped here.
    Failed,
    /// Paused on a question only a human can answer.
    NeedsInput,
    /// Paused at a gate that waits for a human decision.
    HumanGate,
}

/// A task in the queue: its text, and what the supervisor concluded about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// Its position in queue order.
    pub id: TaskId,
    /// What the supervisor concluded about it.
    pub status: TaskStatus,
    /// The task block as authored, headings and all.
    pub body: String,
    /// The `Outcome:` section — the change a reader should be able to name.
    pub outcome: String,
    /// The `Done-when:` section — the observable fact that finishes the task.
    pub done_when: String,
    /// The `Verify:` section — the command that proves it mechanically.
    pub verify: String,
    /// The `Refs:` section — the documents the task is answerable to.
    pub refs: String,
}

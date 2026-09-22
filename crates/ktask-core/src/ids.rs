//! Identifier newtypes: `TaskId`, `AttemptId` and `EventSeq`.
//!
//! Each wraps a bare integer so a task, an attempt and a journal position
//! can never be confused for one another or for an arbitrary `u32`/`u64`.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A task's position in the queue, 1-based and matching queue order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(pub u32);

impl TaskId {
    /// Builds a `TaskId` from its raw value.
    #[must_use]
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw value.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An attempt's position within a task, 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttemptId(pub u32);

impl AttemptId {
    /// Builds an `AttemptId` from its raw value.
    #[must_use]
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw value.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A monotonic, global position in the event journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventSeq(pub u64);

impl EventSeq {
    /// Builds an `EventSeq` from its raw value.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for EventSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_displays_its_raw_value() {
        assert_eq!(TaskId::new(7).to_string(), "7");
    }

    #[test]
    fn attempt_id_displays_its_raw_value() {
        assert_eq!(AttemptId::new(2).to_string(), "2");
    }

    #[test]
    fn event_seq_displays_its_raw_value() {
        assert_eq!(EventSeq::new(1_234).to_string(), "1234");
    }

    #[test]
    fn task_id_round_trips_through_json() {
        let id = TaskId::new(42);
        let json = serde_json::to_string(&id).expect("serialize");
        let back: TaskId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn attempt_id_round_trips_through_json() {
        let id = AttemptId::new(3);
        let json = serde_json::to_string(&id).expect("serialize");
        let back: AttemptId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn event_seq_round_trips_through_json() {
        let seq = EventSeq::new(9_876_543_210);
        let json = serde_json::to_string(&seq).expect("serialize");
        let back: EventSeq = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(seq, back);
    }

    #[test]
    fn get_returns_the_raw_value() {
        assert_eq!(TaskId::new(5).get(), 5);
        assert_eq!(AttemptId::new(6).get(), 6);
        assert_eq!(EventSeq::new(7).get(), 7);
    }
}

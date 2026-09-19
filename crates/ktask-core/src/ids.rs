//! Identifier types: `TaskId`, `AttemptId`, `EventSeq`.

use serde::{Deserialize, Serialize};
use std::fmt;

/// 1-based task identifier, matches queue order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(pub u32);

impl TaskId {
    /// Create a new `TaskId`.
    #[must_use]
    pub fn new(id: u32) -> Self {
        TaskId(id)
    }

    /// Get the underlying u32 value.
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

/// 1-based attempt identifier within a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttemptId(pub u32);

impl AttemptId {
    /// Create a new `AttemptId`.
    #[must_use]
    pub fn new(id: u32) -> Self {
        AttemptId(id)
    }

    /// Get the underlying u32 value.
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

/// Monotonic global event sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventSeq(pub u64);

impl EventSeq {
    /// Create a new `EventSeq`.
    #[must_use]
    pub fn new(seq: u64) -> Self {
        EventSeq(seq)
    }

    /// Get the underlying u64 value.
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
    fn task_id_display() {
        let id = TaskId::new(42);
        assert_eq!(id.to_string(), "42");
    }

    #[test]
    fn task_id_json_roundtrip() {
        let id = TaskId::new(42);
        let json = serde_json::to_string(&id).expect("serialize");
        let deserialized: TaskId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, deserialized);
    }

    #[test]
    fn attempt_id_display() {
        let id = AttemptId::new(7);
        assert_eq!(id.to_string(), "7");
    }

    #[test]
    fn attempt_id_json_roundtrip() {
        let id = AttemptId::new(7);
        let json = serde_json::to_string(&id).expect("serialize");
        let deserialized: AttemptId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, deserialized);
    }

    #[test]
    fn event_seq_display() {
        let seq = EventSeq::new(999);
        assert_eq!(seq.to_string(), "999");
    }

    #[test]
    fn event_seq_json_roundtrip() {
        let seq = EventSeq::new(999);
        let json = serde_json::to_string(&seq).expect("serialize");
        let deserialized: EventSeq = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(seq, deserialized);
    }

    #[test]
    fn ids_formatting_and_roundtrip() {
        // Test Display formatting for all three
        assert_eq!(TaskId::new(1).to_string(), "1");
        assert_eq!(AttemptId::new(2).to_string(), "2");
        assert_eq!(EventSeq::new(3).to_string(), "3");

        // Test JSON round-trip for all three
        let task_id = TaskId::new(100);
        let attempt_id = AttemptId::new(50);
        let event_seq = EventSeq::new(1000);

        let task_json = serde_json::to_string(&task_id).unwrap();
        let attempt_json = serde_json::to_string(&attempt_id).unwrap();
        let event_json = serde_json::to_string(&event_seq).unwrap();

        let task_deser: TaskId = serde_json::from_str(&task_json).unwrap();
        let attempt_deser: AttemptId = serde_json::from_str(&attempt_json).unwrap();
        let event_deser: EventSeq = serde_json::from_str(&event_json).unwrap();

        assert_eq!(task_id, task_deser);
        assert_eq!(attempt_id, attempt_deser);
        assert_eq!(event_seq, event_deser);
    }
}

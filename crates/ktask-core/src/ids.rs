//! The identifiers that name a task, an attempt and a journal record.
//!
//! Each is a newtype over its own integer width rather than a bare number, so
//! the compiler catches the substitution a bare `u32` cannot: an `AttemptId`
//! passed where a `TaskId` was asked for, or a task number written into the
//! `u64` slot a journal sequence occupies.
//!
//! The `serde` derives are what keep durable data plain: an id is written to
//! the journal and to `--json` output as the number it wraps, so a reader —
//! the TUI, a shell pipeline, a later supervisor — never has to know the
//! wrapper exists, and the on-disk format survives the newtypes.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A task in the queue, identified by its 1-based position in queue order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(
    /// The 1-based position of the task in the queue.
    pub u32,
);

impl TaskId {
    /// The id of the task at position `value` in the queue.
    #[must_use]
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// The queue position this id names.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for TaskId {
    /// The bare queue position. A prefix such as `task ` belongs to the
    /// interface printing the id, not to the identifier itself.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One run of a single task, 1-based within that task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttemptId(
    /// The 1-based number of the attempt within its task.
    pub u32,
);

impl AttemptId {
    /// The id of the `value`th run of a task.
    #[must_use]
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// The attempt number within the task.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for AttemptId {
    /// The bare attempt number; see [`TaskId`]'s implementation for why.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The position of a record in the event journal. Monotonic and global — it
/// spans every task and attempt, which is why it is wider than the other two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventSeq(
    /// The global, monotonic sequence number of a journal record.
    pub u64,
);

impl EventSeq {
    /// The id of the journal record at sequence `value`.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The sequence number of the record.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for EventSeq {
    /// The bare sequence number; see [`TaskId`]'s implementation for why.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{AttemptId, EventSeq, TaskId};

    #[test]
    fn ids_display_the_bare_number_and_nothing_else() {
        assert_eq!(TaskId::new(12).to_string(), "12");
        assert_eq!(AttemptId::new(3).to_string(), "3");
        assert_eq!(EventSeq::new(4_294_967_296).to_string(), "4294967296");
    }

    #[test]
    fn ids_round_trip_through_json_as_the_number_they_wrap() {
        let task = TaskId::new(7);
        let encoded = serde_json::to_string(&task).expect("an id is serialisable");
        assert_eq!(encoded, "7");
        assert_eq!(
            serde_json::from_str::<TaskId>(&encoded).expect("the encoding is read back"),
            task
        );

        let attempt = AttemptId::new(2);
        let encoded = serde_json::to_string(&attempt).expect("an id is serialisable");
        assert_eq!(encoded, "2");
        assert_eq!(
            serde_json::from_str::<AttemptId>(&encoded).expect("the encoding is read back"),
            attempt
        );

        let seq = EventSeq::new(9_000_000_000);
        let encoded = serde_json::to_string(&seq).expect("a sequence is serialisable");
        assert_eq!(encoded, "9000000000");
        assert_eq!(
            serde_json::from_str::<EventSeq>(&encoded).expect("the encoding is read back"),
            seq
        );
    }

    #[test]
    fn ids_are_read_back_in_their_own_width_from_a_json_array() {
        let encoded = serde_json::to_string(&EventSeq::new(u64::MAX))
            .expect("a maximum sequence is serialisable");
        assert_eq!(encoded, "18446744073709551615");
        let decoded: Vec<EventSeq> = serde_json::from_str("[18446744073709551615, 4294967296]")
            .expect("a sequence wider than a u32 is read back");
        assert_eq!(
            decoded,
            vec![EventSeq::new(u64::MAX), EventSeq::new(4_294_967_296)]
        );
    }

    #[test]
    fn a_new_id_reports_the_number_it_was_made_from() {
        assert_eq!(TaskId::new(0).get(), 0);
        assert_eq!(TaskId::new(u32::MAX).get(), u32::MAX);
        assert_eq!(AttemptId::new(4).get(), 4);
        assert_eq!(EventSeq::new(u64::MAX).get(), u64::MAX);
    }

    #[test]
    fn ids_of_the_same_number_are_equal_and_order_by_that_number() {
        assert_eq!(TaskId::new(3), TaskId::new(3));
        assert_ne!(TaskId::new(3), TaskId::new(4));
        assert!(TaskId::new(3) < TaskId::new(4));
        assert!(EventSeq::new(11) > EventSeq::new(10));
        assert!(AttemptId::new(1) <= AttemptId::new(1));
    }
}

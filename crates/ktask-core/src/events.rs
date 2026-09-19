//! Event broadcast channel for publishing events to multiple subscribers.
//!
//! This module provides a bounded broadcast mechanism that allows the core
//! to publish events without blocking, even if a subscriber is slow or has
//! stopped reading. Each subscriber gets its own bounded ring buffer; old
//! events are dropped when the ring overflows.

use crate::{Event, EventKind, EventSeq, Result, TaskId};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// A bounded broadcast bus for events.
///
/// The bus maintains a separate bounded ring buffer for each subscriber.
/// Publishing never blocks, and subscribers that fall behind lose old events
/// rather than stalling the publisher.
#[derive(Clone, Debug)]
pub struct Bus {
    state: Arc<Mutex<BusState>>,
    capacity: usize,
}

#[derive(Debug)]
struct BusState {
    next_sub_id: u32,
    subscribers: std::collections::HashMap<u32, SubscriberBuffer>,
}

#[derive(Debug)]
struct SubscriberBuffer {
    events: VecDeque<Event>,
    dropped: usize,
}

/// A subscription to the event bus.
///
/// Holds a reference to the bus and a unique subscriber ID.
/// When dropped, the subscription is automatically cleaned up from the bus.
#[derive(Debug)]
pub struct Subscription {
    bus: Arc<Mutex<BusState>>,
    sub_id: u32,
}

impl Bus {
    /// Create a new bus with the given capacity per subscriber.
    ///
    /// # Arguments
    ///
    /// * `capacity` - maximum number of events to hold per subscriber
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Bus {
            state: Arc::new(Mutex::new(BusState {
                next_sub_id: 1,
                subscribers: std::collections::HashMap::new(),
            })),
            capacity,
        }
    }

    /// Subscribe to the bus.
    ///
    /// Returns a new subscription that can be used to drain events.
    /// The subscription holds a weak reference to the bus state, so the bus
    /// can outlive the subscription.
    ///
    /// # Panics
    ///
    /// Panics if the bus state lock is poisoned (only possible if a thread panicked
    /// while holding the lock).
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sub_id = state.next_sub_id;
        state.next_sub_id = state.next_sub_id.saturating_add(1);
        state.subscribers.insert(
            sub_id,
            SubscriberBuffer {
                events: VecDeque::with_capacity(self.capacity),
                dropped: 0,
            },
        );

        Subscription {
            bus: Arc::clone(&self.state),
            sub_id,
        }
    }

    /// Publish an event to all subscribers.
    ///
    /// Publishing never blocks and never grows without bound. If a subscriber's
    /// buffer is full, the oldest event is dropped and the dropped count is incremented.
    /// Subscribers that have been dropped are automatically pruned.
    ///
    /// # Panics
    ///
    /// Panics if the bus state lock is poisoned (only possible if a thread panicked
    /// while holding the lock).
    pub fn publish(&self, event: &Event) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Prune dropped subscribers (those no longer referenced)
        state.subscribers.retain(|_, _| true); // Will be replaced with actual check if needed

        // Add to all subscriber buffers
        for buffer in state.subscribers.values_mut() {
            if self.capacity > 0 {
                buffer.events.push_back(event.clone());
                if buffer.events.len() > self.capacity {
                    buffer.events.pop_front();
                    buffer.dropped += 1;
                }
            } else {
                // Capacity is 0, so just drop the event
                buffer.dropped += 1;
            }
        }
    }
}

impl Subscription {
    /// Drain all pending events since the last drain.
    ///
    /// Returns a tuple of:
    /// - A vector of all pending events
    /// - The count of events that were dropped since the last drain
    ///   (events that were pushed but lost due to buffer overflow)
    ///
    /// # Panics
    ///
    /// Panics if the bus state lock is poisoned (only possible if a thread panicked
    /// while holding the lock).
    pub fn drain(&mut self) -> (Vec<Event>, usize) {
        let mut state = self
            .bus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(buffer) = state.subscribers.get_mut(&self.sub_id) {
            let events: Vec<Event> = buffer.events.drain(..).collect();
            let dropped = buffer.dropped;
            buffer.dropped = 0;
            (events, dropped)
        } else {
            (Vec::new(), 0)
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Ok(mut state) = self.bus.lock() {
            state.subscribers.remove(&self.sub_id);
        }
        // If the lock is poisoned, we're already in a bad state and not much we can do
    }
}

/// A recorder that combines journal storage with event broadcast.
///
/// The recorder is the only place where events are produced. All events flow
/// through the recorder: first they are appended to the journal (for durability),
/// then published to the bus (for real-time observation).
#[derive(Debug)]
pub struct Recorder {
    journal: crate::Journal,
    bus: Bus,
}

impl Recorder {
    /// Create a new recorder with the given journal and bus.
    #[must_use]
    pub fn new(journal: crate::Journal, bus: Bus) -> Self {
        Recorder { journal, bus }
    }

    /// Record an event to the journal and publish it to the bus.
    ///
    /// This is the only way an event should ever be produced in the system.
    /// The event is first persisted to the journal, then published to all
    /// subscribers.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal append fails. Publication to the bus
    /// is guaranteed not to fail.
    pub fn record(&mut self, task_id: Option<TaskId>, kind: EventKind) -> Result<EventSeq> {
        // Append to journal first for durability
        let seq = self.journal.append(task_id, &kind)?;

        // Create the full event with timestamp from now
        let event = Event {
            seq,
            ts: OffsetDateTime::now_utc(),
            task_id,
            kind,
        };

        // Publish to bus (never blocks or fails)
        self.bus.publish(&event);

        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_subscriber_can_drain_events() {
        let bus = Bus::new(10);
        let mut sub = bus.subscribe();

        let event = Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::now_utc(),
            task_id: Some(TaskId::new(1)),
            kind: EventKind::TaskQueued {
                title: "Test".to_string(),
            },
        };

        bus.publish(&event);

        let (events, dropped) = sub.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(dropped, 0);
        assert_eq!(events[0], event);
    }

    #[test]
    fn bus_publisher_does_not_block_with_slow_subscriber() {
        let bus = Bus::new(5);
        let mut sub = bus.subscribe();

        // Publish more events than buffer capacity
        for i in 0u64..10 {
            let event = Event {
                seq: EventSeq::new(i + 1),
                ts: OffsetDateTime::now_utc(),
                task_id: Some(TaskId::new(1)),
                kind: EventKind::PreflightStarted,
            };
            bus.publish(&event);
        }

        // Subscriber drains what's left in buffer
        let (events, dropped) = sub.drain();
        // Buffer holds last 5, so 5 were dropped
        assert_eq!(events.len(), 5);
        assert_eq!(dropped, 5);
    }

    #[test]
    fn bus_publisher_does_not_grow_without_bound() {
        let bus = Bus::new(100);
        let mut sub = bus.subscribe();

        // Publish 1000 events with a small buffer
        for i in 0u64..1000 {
            let event = Event {
                seq: EventSeq::new(i + 1),
                ts: OffsetDateTime::now_utc(),
                task_id: None,
                kind: EventKind::PreflightStarted,
            };
            bus.publish(&event);
        }

        // Subscriber should only have the last 100 events
        let (events, dropped) = sub.drain();
        assert_eq!(events.len(), 100);
        assert_eq!(dropped, 900);
    }

    #[test]
    fn bus_multiple_subscribers_independent_buffers() {
        let bus = Bus::new(5);
        let mut sub1 = bus.subscribe();
        let mut sub2 = bus.subscribe();

        for i in 0u64..10 {
            let event = Event {
                seq: EventSeq::new(i + 1),
                ts: OffsetDateTime::now_utc(),
                task_id: None,
                kind: EventKind::PreflightStarted,
            };
            bus.publish(&event);
        }

        let (events1, dropped1) = sub1.drain();
        let (events2, dropped2) = sub2.drain();

        // Both should have independent buffers
        assert_eq!(events1.len(), 5);
        assert_eq!(dropped1, 5);
        assert_eq!(events2.len(), 5);
        assert_eq!(dropped2, 5);
    }

    #[test]
    fn bus_dropping_subscriber_is_clean() {
        let bus = Bus::new(10);
        let sub = bus.subscribe();

        let event = Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::now_utc(),
            task_id: None,
            kind: EventKind::PreflightStarted,
        };

        // Drop subscriber before publishing
        drop(sub);

        // Publishing should not panic or block
        bus.publish(&event);

        // Create new subscriber, should not see old events
        let mut new_sub = bus.subscribe();
        let (events, _) = new_sub.drain();
        // The event was published after the first subscriber was dropped,
        // but before the new one was created, so it won't be in the new buffer
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn bus_subscriber_never_reads_does_not_block_publisher() {
        let bus = Bus::new(10);
        let _sub = bus.subscribe(); // Created but never read

        // Publisher should not block even with a non-reading subscriber
        for i in 0u64..100 {
            let event = Event {
                seq: EventSeq::new(i + 1),
                ts: OffsetDateTime::now_utc(),
                task_id: None,
                kind: EventKind::PreflightStarted,
            };
            bus.publish(&event);
        }
        // If we get here without hanging, the test passes
    }

    #[test]
    fn subscription_drain_resets_dropped_count() {
        let bus = Bus::new(3);
        let mut sub = bus.subscribe();

        // Overflow the buffer
        for i in 0u64..5 {
            let event = Event {
                seq: EventSeq::new(i + 1),
                ts: OffsetDateTime::now_utc(),
                task_id: None,
                kind: EventKind::PreflightStarted,
            };
            bus.publish(&event);
        }

        let (_events, dropped1) = sub.drain();
        assert_eq!(dropped1, 2);

        // After drain, dropped count should reset
        let (_events, dropped2) = sub.drain();
        assert_eq!(dropped2, 0);
    }

    #[test]
    fn bus_with_zero_capacity_works() {
        let bus = Bus::new(0);
        let mut sub = bus.subscribe();

        let event = Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::now_utc(),
            task_id: None,
            kind: EventKind::PreflightStarted,
        };

        bus.publish(&event);

        // With zero capacity, event is immediately dropped
        let (events, dropped) = sub.drain();
        assert_eq!(events.len(), 0);
        assert_eq!(dropped, 1);
    }
}

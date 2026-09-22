//! `Bus`: the broadcast channel through which frontends observe a run.
//!
//! `ktask-core` does no terminal I/O (VISION.md section 5): the CLI and the
//! TUI both learn what happened by subscribing here. Each subscriber owns a
//! bounded ring of its own, so a frontend that stops reading — a detached
//! TUI, a CLI command that only cares about the final exit code — can never
//! make [`Bus::publish`] block or the bus grow without bound. It only loses
//! its own oldest undelivered events, and knows how many it lost.
//!
//! [`Recorder`] is the only thing allowed to call [`Journal::append`] or
//! [`Bus::publish`]: appending to the journal and publishing the same event
//! happen together, so nothing can reach a frontend that is not already
//! durable, and nothing durable goes unannounced.

use crate::{Event, EventKind, EventSeq, Journal, Result, TaskId};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use time::OffsetDateTime;

/// Locks `mutex`, recovering the guard from a poisoned lock rather than
/// panicking: a panicking publisher would take the whole bus down with the
/// task that panicked, which is exactly the failure this module exists to
/// avoid.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One subscriber's private, bounded queue of undelivered events.
#[derive(Debug)]
struct Ring {
    capacity: usize,
    events: VecDeque<Event>,
    dropped: usize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Ring {
            capacity,
            events: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Pushes `event`, evicting the oldest queued event and counting it as
    /// dropped when the ring is already at `capacity`. A `capacity` of zero
    /// drops every event immediately.
    fn push(&mut self, event: Event) {
        if self.capacity == 0 {
            self.dropped += 1;
            return;
        }
        if self.events.len() == self.capacity {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(event);
    }

    fn drain(&mut self) -> (Vec<Event>, usize) {
        let events = self.events.drain(..).collect();
        let dropped = std::mem::take(&mut self.dropped);
        (events, dropped)
    }
}

/// The broadcast channel from a run to every attached frontend.
///
/// Holds one bounded ring per live [`Subscription`], each capped at the
/// capacity given to [`Bus::new`]. Dropping a `Subscription` is clean: the
/// bus holds only a [`Weak`] reference to its ring, so the next
/// [`Bus::publish`] prunes it without the subscriber having to unregister.
#[derive(Debug)]
pub struct Bus {
    capacity: usize,
    subscribers: Mutex<Vec<Weak<Mutex<Ring>>>>,
}

impl Bus {
    /// Creates a bus whose subscribers each buffer up to `capacity` events
    /// before the oldest starts being dropped.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Bus {
            capacity,
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Attaches a new subscriber with an empty ring, returning a handle it
    /// can [`Subscription::drain`].
    ///
    /// The new subscriber sees only events published after it subscribes;
    /// whatever was published before is already gone.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        let ring = Arc::new(Mutex::new(Ring::new(self.capacity)));
        lock(&self.subscribers).push(Arc::downgrade(&ring));
        Subscription { ring }
    }

    /// Publishes `e` to every live subscriber's ring.
    ///
    /// Never blocks: each ring is only ever held long enough to push one
    /// event and, if it was already full, evict the oldest. Subscribers
    /// whose [`Subscription`] has since been dropped are pruned from the
    /// bus here, so a bus with no live subscribers left holds nothing.
    pub fn publish(&self, e: Event) {
        let live: Vec<Arc<Mutex<Ring>>> = {
            let mut subs = lock(&self.subscribers);
            let mut live = Vec::with_capacity(subs.len());
            subs.retain(|weak| match weak.upgrade() {
                Some(ring) => {
                    live.push(ring);
                    true
                }
                None => false,
            });
            live
        };

        let Some((last, rest)) = live.split_last() else {
            return;
        };
        for ring in rest {
            lock(ring).push(e.clone());
        }
        lock(last).push(e);
    }

    /// Returns how many subscribers currently have a live handle.
    ///
    /// Test-only: it exists to prove that [`Bus::publish`] prunes dropped
    /// subscriptions rather than accumulating dead entries forever.
    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        lock(&self.subscribers).len()
    }
}

/// A live attachment to a [`Bus`], returned by [`Bus::subscribe`].
///
/// Dropping a `Subscription` — including by letting it go out of scope — is
/// the entire unsubscribe protocol; there is nothing else to call.
#[derive(Debug)]
pub struct Subscription {
    ring: Arc<Mutex<Ring>>,
}

impl Subscription {
    /// Returns every event published since the last call to `drain` (or
    /// since subscribing, on the first call), and how many older events
    /// were evicted in the meantime to keep this ring bounded.
    pub fn drain(&mut self) -> (Vec<Event>, usize) {
        lock(&self.ring).drain()
    }
}

/// Appends every event a run produces to the journal, then publishes it.
///
/// `Recorder` is the only path by which an event is ever produced: nothing
/// else calls [`Journal::append`] or [`Bus::publish`] directly, so an event
/// can never reach a subscriber before it is journaled, and never be
/// journaled without also being announced.
#[derive(Debug)]
pub struct Recorder {
    journal: Journal,
    bus: Bus,
}

impl Recorder {
    /// Wraps `journal`, publishing everything it records to `bus`.
    #[must_use]
    pub fn new(journal: Journal, bus: Bus) -> Self {
        Recorder { journal, bus }
    }

    /// Attaches a new subscriber to this recorder's bus.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        self.bus.subscribe()
    }

    /// Appends `kind` to the journal under `task`, then publishes the
    /// resulting event to every live subscriber.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Journal::append`] returns on failure to append.
    pub fn record(&mut self, task: Option<TaskId>, kind: EventKind) -> Result<EventSeq> {
        let seq = self.journal.append(task, &kind)?;
        self.bus.publish(Event {
            seq,
            ts: OffsetDateTime::now_utc(),
            task_id: task,
            kind,
        });
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn event(seq: u64, kind: EventKind) -> Event {
        Event {
            seq: EventSeq::new(seq),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: None,
            kind,
        }
    }

    #[test]
    fn a_subscriber_that_never_drains_does_not_block_publish_or_grow_without_bound() {
        let bus = Bus::new(8);
        let mut sub = bus.subscribe();

        for i in 0..1_000 {
            bus.publish(event(i, EventKind::Resumed));
        }

        let (events, dropped) = sub.drain();
        assert_eq!(events.len(), 8, "ring must stay capped at capacity");
        assert_eq!(dropped, 1_000 - 8, "everything evicted must be counted");
    }

    #[test]
    fn drain_returns_events_in_publish_order() {
        let bus = Bus::new(4);
        let mut sub = bus.subscribe();

        bus.publish(event(1, EventKind::Resumed));
        bus.publish(event(2, EventKind::PreflightStarted));

        let (events, dropped) = sub.drain();
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, EventSeq::new(1));
        assert_eq!(events[1].seq, EventSeq::new(2));
    }

    #[test]
    fn drain_resets_the_dropped_count_and_empties_the_ring() {
        let bus = Bus::new(2);
        let mut sub = bus.subscribe();

        for i in 0..5 {
            bus.publish(event(i, EventKind::Resumed));
        }
        let (_events, first_dropped) = sub.drain();
        assert_eq!(first_dropped, 3);

        let (events, second_dropped) = sub.drain();
        assert_eq!(
            events,
            Vec::new(),
            "a second drain with nothing new is empty"
        );
        assert_eq!(second_dropped, 0, "dropped count must not carry over");
    }

    #[test]
    fn a_subscriber_only_sees_events_published_after_it_subscribed() {
        let bus = Bus::new(8);
        bus.publish(event(1, EventKind::Resumed));

        let mut sub = bus.subscribe();
        bus.publish(event(2, EventKind::PreflightStarted));

        let (events, dropped) = sub.drain();
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, EventSeq::new(2));
    }

    #[test]
    fn each_subscriber_gets_its_own_independent_ring() {
        let bus = Bus::new(8);
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();

        bus.publish(event(1, EventKind::Resumed));

        let (first_events, _) = first.drain();
        let (second_events, _) = second.drain();
        assert_eq!(first_events.len(), 1);
        assert_eq!(second_events.len(), 1);

        // Draining one must not affect the other's future reads.
        bus.publish(event(2, EventKind::PreflightStarted));
        let (first_events, _) = first.drain();
        assert_eq!(first_events.len(), 1);
        assert_eq!(first_events[0].seq, EventSeq::new(2));
    }

    #[test]
    fn dropping_a_subscription_is_pruned_from_the_bus_on_the_next_publish() {
        let bus = Bus::new(8);
        let sub = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        drop(sub);
        assert_eq!(
            bus.subscriber_count(),
            1,
            "pruning happens on publish, not on drop"
        );

        bus.publish(event(1, EventKind::Resumed));
        assert_eq!(
            bus.subscriber_count(),
            0,
            "a publish after the drop must prune the dead entry"
        );
    }

    #[test]
    fn publishing_with_no_subscribers_does_nothing_and_does_not_panic() {
        let bus = Bus::new(8);
        bus.publish(event(1, EventKind::Resumed));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn recorder_records_to_the_journal_and_publishes_a_matching_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).expect("open journal");

        let mut recorder = Recorder::new(journal, Bus::new(8));
        let mut sub = recorder.subscribe();

        let kind = EventKind::TaskQueued {
            title: "Add widget".to_string(),
        };
        let seq = recorder
            .record(Some(TaskId::new(1)), kind.clone())
            .expect("record");

        let (events, dropped) = sub.drain();
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, seq);
        assert_eq!(events[0].task_id, Some(TaskId::new(1)));
        assert_eq!(events[0].kind, kind);

        // Re-open the same file, independent of the `Recorder` that wrote
        // it, to prove the append actually landed in the journal under the
        // same sequence number, task and kind as what was published.
        let journal = Journal::open(&path).expect("reopen journal");
        let stored = journal.events().expect("events");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].seq, seq);
        assert_eq!(stored[0].task_id, Some(TaskId::new(1)));
        assert_eq!(stored[0].kind, kind);
    }

    #[test]
    fn recorder_assigns_strictly_increasing_sequence_numbers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).expect("open journal");

        let mut recorder = Recorder::new(journal, Bus::new(8));
        let first = recorder.record(None, EventKind::Resumed).expect("record 1");
        let second = recorder
            .record(None, EventKind::PreflightStarted)
            .expect("record 2");

        assert!(first.get() < second.get());
    }
}

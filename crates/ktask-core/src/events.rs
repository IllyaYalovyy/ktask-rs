//! Who is told, after what is written: the event bus and the one door into it.
//!
//! VISION.md section 5 keeps terminal I/O out of this crate — "progress and events
//! flow through typed channels/callbacks", with both frontends consuming the same
//! stream so that everything the TUI shows stays scriptable from the CLI. [`Bus`]
//! is that stream. It is neither a queue nor a record: it holds an event only until
//! the subscriber behind it reads it, and it hands nothing at all to a subscriber
//! that arrived after the event was published. The durable history is the journal,
//! and a frontend that needs the past reads it ([`Journal::events_since`],
//! [`Journal::for_each_event`]) instead of asking a ring that was never sized to
//! keep a run.
//!
//! # A bounded ring per subscriber, drop-oldest
//!
//! `docs/DESIGN.md` fixes the shape: capacity `output_ring_lines`, drop-oldest on
//! overflow, and a `dropped: usize` count the interface can display. The shape is
//! what makes a slow frontend a display problem rather than an operational one. A
//! subscriber that stops reading — a render that fell behind, a screen nobody is
//! looking at, a pipe nobody is scrolling — is not allowed to decide how much
//! memory a run costs or whether the run proceeds, so the publisher never waits for
//! it and never keeps what it did not take: the newest `capacity` events stay,
//! older ones are given up, and the number given up is handed over with them. A
//! screen that lost events is then able to say so instead of looking complete,
//! which is the difference between a truncated view and a false one.
//!
//! Every subscriber gets its own ring and its own copy of each event, so one
//! frontend draining at 60 Hz does not starve a CLI printing each record once.
//!
//! # Publishing never blocks
//!
//! A ring is bounded and its push runs no foreign code, so the work a publisher
//! does per subscriber is fixed and does not depend on whether anybody is reading.
//! The shared subscriber list is held only to prune and to collect the live rings,
//! and no code a caller or a frontend owns runs under either lock — which is also
//! why a mutex poisoned by a panic elsewhere is recovered rather than propagated:
//! unwrapping here would turn one thread's bug into a supervisor that can no longer
//! record anything.
//!
//! # The one door
//!
//! [`Recorder::record`] is how an event comes to exist: it appends to the journal
//! and then publishes the record the journal wrote. Nothing else calls
//! [`Journal::append`] or [`Bus::publish`]. The order is the point — VISION.md
//! section 3 journals every transition before the side effect it describes — so an
//! event a frontend can see but a replay cannot is not merely possible here, it is
//! the thing this type exists to make impossible. And because the journal owns the
//! sequence and the instant (ADR-0016), what reaches a subscriber is the stored
//! envelope read back through [`Journal::event`] rather than a second copy of what
//! the caller expected to have written.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use crate::{Config, Error, Event, EventKind, EventSeq, Journal, Result, TaskId};

/// How long the tests wait for a publish loop to finish, so that a publisher which
/// waits for a reader fails the suite instead of hanging it. Generous by three
/// orders of magnitude: the loop it times takes milliseconds.
#[cfg(test)]
const PUBLISH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Take a lock, keeping the bus usable if some other thread panicked while holding
/// it.
///
/// Nothing in this module calls code it does not own with a lock held, so a
/// poisoned mutex here means a bug inside this module rather than a caller's — and
/// the failure mode the crate refuses is the supervisor's, not the bug's: a bus that
/// stops publishing stops every frontend with it, and a run whose screens went dark
/// is worse than a ring whose counts came from a broken build.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One subscriber's unread events, newest kept and oldest given up.
#[derive(Debug)]
struct Ring {
    /// How many events this ring holds at once, from `output_ring_lines`.
    capacity: usize,
    /// The events themselves, oldest at the front.
    events: VecDeque<Event>,
    /// How many events this ring has dropped to stay inside `capacity`.
    dropped: usize,
}

impl Ring {
    /// An empty ring that holds at most `capacity` events.
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            events: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Add one event, giving up the oldest until the ring is inside its capacity.
    fn push(&mut self, event: Event) {
        self.events.push_back(event);
        // One push can overshoot by one event at most, so one event is given up —
        // and the count moves only when something really was taken out, which is
        // what lets a capacity of zero report a true number rather than a guess.
        if self.events.len() > self.capacity && self.events.pop_front().is_some() {
            self.dropped += 1;
        }
    }

    /// Take everything unread and the count of what was dropped to hold it.
    fn take(&mut self) -> (Vec<Event>, usize) {
        (
            std::mem::take(&mut self.events).into(),
            std::mem::take(&mut self.dropped),
        )
    }
}

/// The live view of a run: every event the core has recorded, handed to whoever is
/// watching without the core knowing or caring who that is.
///
/// Construct one per run and hand it out with [`Bus::subscribe`]; the frontends
/// need not know about each other, and a frontend that comes and goes costs one
/// ring that is pruned on the next publish. A [`Recorder`] owns the bus a run's
/// events actually go through, which is the only way an event is produced.
#[derive(Clone)]
pub struct Bus {
    /// The per-subscriber capacity every ring this bus creates is born with.
    capacity: usize,
    /// One slot per subscriber, held weakly so that the subscriber itself is what
    /// keeps its ring alive.
    slots: Arc<Mutex<Vec<Weak<Mutex<Ring>>>>>,
}

impl Bus {
    /// A bus whose rings are the size `docs/DESIGN.md` gives `output_ring_lines`.
    ///
    /// The default comes from [`Config::default()`] rather than a number repeated
    /// here, so the document's one source of defaults stays the only place that
    /// size is written down.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(Config::default().output_ring_lines)
    }

    /// A bus whose rings hold at most `capacity` unread events each.
    ///
    /// `0` is answerable rather than refused: every published event is given up at
    /// once and counted, so a configuration that asks for no ring gets an honest
    /// "everything was dropped" rather than a panic or a buffer that quietly grew.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            slots: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Open a view of the run: from now until the returned value is dropped, this
    /// subscriber is told every event that is published.
    ///
    /// What was published before the call is not replayed. The bus carries a run to
    /// whoever is watching; the journal is where the past is read.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        let ring = Arc::new(Mutex::new(Ring::new(self.capacity)));
        locked(&self.slots).push(Arc::downgrade(&ring));
        Subscription { ring }
    }

    /// Hand `e` to every subscriber alive at this moment, and to nobody else.
    ///
    /// The call does not wait for a subscriber to read, does not grow with the
    /// number of events published, and does not fail: an event that reached no
    /// subscriber is still in the journal, which is where the record of the run
    /// lives. [`Recorder`] is what publishes, and it is the only thing that does.
    pub fn publish(&self, e: Event) {
        let mut slots = locked(&self.slots);
        let mut alive: Vec<Arc<Mutex<Ring>>> = Vec::with_capacity(slots.len());
        // Pruning happens on the way past: a slot whose subscriber has been dropped
        // is removed here, so a frontend that opened and closed a view a thousand
        // times over a long run leaves nothing behind. `alive` is filled in the same
        // pass, under the same lock, so no view can open or close between the two
        // and be missed by the publish below.
        slots.retain(|slot| match slot.upgrade() {
            Some(ring) => {
                alive.push(ring);
                true
            }
            None => false,
        });
        drop(slots);
        // The caller's own copy goes to the last subscriber and the earlier ones get
        // clones, so no two rings share one event's lifetime and the event handed in
        // is moved rather than thrown away.
        if let Some((last, earlier)) = alive.split_last() {
            for ring in earlier {
                locked(ring).push(e.clone());
            }
            locked(last).push(e);
        }
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Bus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Counts, not contents: a bus can be holding `output_ring_lines` events per
        // subscriber, and a `Debug` that dumped them would make the largest thing in
        // a panic report the very view someone was trying to look at.
        f.debug_struct("Bus")
            .field("capacity", &self.capacity)
            .field("slots", &locked(&self.slots).len())
            .finish()
    }
}

/// One subscriber's view of the run, and the count of what it has missed.
///
/// Kept alive by whoever holds it: dropping it ends the view and its ring, and the
/// next [`Bus::publish`] prunes the slot. Cloning one subscription would be a second
/// reader of the same ring silently dividing the events between itself, so there is
/// no `Clone`: two readers take two subscriptions.
pub struct Subscription {
    /// The ring this view reads, shared with the [`Bus`] that publishes into it.
    ring: Arc<Mutex<Ring>>,
}

impl Subscription {
    /// Take every event published since this call last ran, oldest first, with the
    /// number of events the ring dropped in the meantime.
    ///
    /// The count covers the same window as the events rather than the life of the
    /// subscription, so a screen can print "N earlier events lost" beside the lines
    /// it did show and start the next window clean. An empty `Vec` with a non-zero
    /// count is the worst case the design allows: nothing to show, and an honest
    /// statement of why.
    #[must_use]
    pub fn drain(&mut self) -> (Vec<Event>, usize) {
        locked(&self.ring).take()
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Counts rather than contents, for the reason `Bus`'s implementation gives
        // above: the ring is sized in `output_ring_lines`, and the thing a developer
        // reaches for in a panic report should not be the largest thing in it.
        let ring = locked(&self.ring);
        f.debug_struct("Subscription")
            .field("capacity", &ring.capacity)
            .field("unread", &ring.events.len())
            .field("dropped", &ring.dropped)
            .finish()
    }
}

/// The one door an event comes through: append it, then publish what was appended.
///
/// Holding the journal and the bus together is what makes the order unskippable —
/// there is no way to be told about a transition that is not in the durable record,
/// and no way for a record to be written that leaves a screen waiting for it. A run
/// owns one recorder; frontends take [`Recorder::subscribe`] and never touch the
/// journal to follow the live run.
#[derive(Debug)]
pub struct Recorder {
    journal: Journal,
    bus: Bus,
}

impl Recorder {
    /// A recorder over `journal`, publishing to rings of the documented default
    /// size.
    #[must_use]
    pub fn new(journal: Journal) -> Self {
        Self {
            journal,
            bus: Bus::new(),
        }
    }

    /// A recorder over `journal` publishing to `bus`.
    ///
    /// The bus is how a run sizes its rings from its own configuration
    /// (`output_ring_lines` read from the project's file rather than the default)
    /// and how a test watches a record with a ring small enough to overflow.
    #[must_use]
    pub fn with_bus(journal: Journal, bus: Bus) -> Self {
        Self { journal, bus }
    }

    /// A view of every event this recorder records from now on.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        self.bus.subscribe()
    }

    /// The journal this recorder appends to, for a step that has to ask it a
    /// question before it records anything.
    ///
    /// `pub(crate)` and shared rather than mutable: [`Journal::append`] needs
    /// `&mut self`, so a handle taken from here cannot write, and the two
    /// triggers that keep the journal append-only are untouched.
    /// [`crate::file_report`] is the caller that needs it — whether one
    /// remediation has accounted for itself already is a question about the
    /// journal, and asking it through a second connection to the same file
    /// would leave two answers available to disagree (ADR-0091).
    #[must_use]
    pub(crate) fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Append one event and publish the record the journal wrote, returning the
    /// sequence the journal gave it.
    ///
    /// The append comes first and the publish only happens once the row is
    /// committed, which is what lets a frontend treat what it sees as something that
    /// happened. What is published is the stored envelope, read back by its
    /// sequence: the sequence the database issued and the instant the append
    /// stamped, not values this call guessed at (ADR-0016).
    ///
    /// # Errors
    ///
    /// As [`Journal::append`]: [`Error::Serde`] when the payload has no JSON
    /// encoding and [`Error::Database`] when SQLite refuses it — and in either case
    /// nothing is published, because nothing happened. [`Error::Corrupt`] when the
    /// record just committed cannot be read back, which says the file is no longer
    /// the file that answered; that, too, publishes nothing.
    pub fn record(&mut self, task: Option<TaskId>, kind: EventKind) -> Result<EventSeq> {
        let seq = self.journal.append(task, &kind)?;
        // The kind the caller handed over is spent by the append above, which encoded
        // it as the row's payload. What is published below is deliberately *not* it:
        // a subscriber sees the envelope the journal wrote, whose sequence and instant
        // are the journal's (ADR-0016).
        drop(kind);
        let stored = stored_event(&self.journal, seq)?;
        self.bus.publish(stored);
        Ok(seq)
    }
}

/// The stored record at `seq`, refused rather than guessed at when it is not there.
///
/// The envelope a subscriber is shown has to come from the file, because the two
/// facts that make it a record — its sequence and its instant — are the journal's
/// own (ADR-0016). Reading it back through [`Journal::event`] rather than from a
/// cursor read is what keeps one recorded event one row of work instead of the tail
/// of the journal, and keeps damage in an unrelated row from refusing a record that
/// was read correctly.
fn stored_event(journal: &Journal, seq: EventSeq) -> Result<Event> {
    journal.event(seq)?.ok_or_else(|| Error::Corrupt {
        detail: "the record just committed is not in the journal that committed it".to_owned(),
        seq: Some(seq.get()),
    })
}

#[cfg(test)]
mod tests {
    use super::{Bus, PUBLISH_DEADLINE, Recorder, locked, stored_event};
    use crate::{Config, Error, Event, EventKind, EventSeq, Journal, TaskId};
    use std::sync::Arc;
    use std::sync::mpsc;
    use tempfile::{TempDir, tempdir};
    use time::macros::datetime;

    /// An event the way a publish receives one.
    ///
    /// The sequence and the instant are this test's own rather than a journal's: a
    /// ring is being asked what it keeps and what it gives up, and it never looks at
    /// either.
    fn published(seq: u64, title: &str) -> Event {
        Event {
            seq: EventSeq::new(seq),
            ts: datetime!(2026-09-17 12:34:56 UTC),
            task_id: Some(TaskId::new(1)),
            kind: EventKind::TaskQueued {
                title: title.to_owned(),
            },
        }
    }

    /// The titles a read handed back, in the order it handed them back — the order
    /// is half of what a ring promises, so most assertions below are against these.
    fn titles(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|event| match &event.kind {
                EventKind::TaskQueued { title } => title.clone(),
                other => panic!("an event this module publishes carries a title: {other:?}"),
            })
            .collect()
    }

    /// The sequences a read handed back, in the order it handed them back.
    fn published_sequences(events: &[Event]) -> Vec<u64> {
        events.iter().map(|event| event.seq.get()).collect()
    }

    /// A journal in a scratch directory, and the directory that keeps it there.
    ///
    /// The [`TempDir`] is returned beside it so a test holds the directory for as
    /// long as it holds the journal: `docs/DESIGN.md` Conventions forbids a test
    /// from writing anywhere in this repository.
    fn a_journal() -> (TempDir, Journal) {
        let parent = tempdir().expect("a scratch directory below the system temp directory");
        let journal =
            Journal::open(&parent.path().join("journal.db")).expect("a new journal opens");
        (parent, journal)
    }

    /// How many subscriber slots a bus is holding, dead ones included until the next
    /// publish notices them.
    fn slots_held(bus: &Bus) -> usize {
        locked(&bus.slots).len()
    }

    #[test]
    fn a_subscriber_is_told_everything_published_after_it_arrived() {
        let bus = Bus::with_capacity(8);
        let mut watcher = bus.subscribe();

        bus.publish(published(1, "first"));
        bus.publish(published(2, "second"));

        let (events, dropped) = watcher.drain();
        assert_eq!(
            titles(&events),
            ["first", "second"],
            "in the order they were published: a run that arrives out of order reads as a \
             different run {events:?}"
        );
        assert_eq!(
            dropped, 0,
            "nothing overflowed a ring that was asked for nothing"
        );
    }

    #[test]
    fn a_subscriber_is_not_told_what_was_published_before_it_arrived() {
        let bus = Bus::with_capacity(8);
        bus.publish(published(1, "before you got here"));
        let mut late = bus.subscribe();

        let (events, dropped) = late.drain();
        assert!(
            events.is_empty(),
            "the bus is a live view and not the journal, so a subscriber that arrived after an \
             event was published is not handed history it never watched being written: {events:?}"
        );
        assert_eq!(
            dropped, 0,
            "an event published before this view existed is not a loss it should be told about"
        );

        bus.publish(published(2, "after"));
        assert_eq!(
            titles(&late.drain().0),
            ["after"],
            "what a late view does get is everything from the moment it opened"
        );
    }

    #[test]
    fn each_subscriber_gets_its_own_copy_and_one_drain_leaves_the_other_full() {
        let bus = Bus::with_capacity(8);
        let mut tui = bus.subscribe();
        let mut cli = bus.subscribe();

        bus.publish(published(1, "seen twice"));

        assert_eq!(
            titles(&tui.drain().0),
            ["seen twice"],
            "the first view read it"
        );
        assert_eq!(
            titles(&cli.drain().0),
            ["seen twice"],
            "and the second still has it: two frontends watching one run show the same events, \
             and neither can empty the other's view by reading"
        );
    }

    #[test]
    fn a_drain_reports_what_arrived_since_the_last_drain_and_nothing_twice() {
        let bus = Bus::with_capacity(8);
        let mut watcher = bus.subscribe();
        bus.publish(published(1, "one"));

        assert_eq!(
            titles(&watcher.drain().0),
            ["one"],
            "the first read reports it"
        );

        bus.publish(published(2, "two"));

        assert_eq!(
            titles(&watcher.drain().0),
            ["two"],
            "the next read reports what came after it and not the record before it again, which \
             is what stops a view polled on every frame from showing each line twice"
        );
        let (again, again_dropped) = watcher.drain();
        assert!(
            again.is_empty() && again_dropped == 0,
            "with nothing published since the last read the honest answer is nothing: \
             {again:?} and {again_dropped} dropped"
        );
    }

    #[test]
    fn a_full_ring_gives_up_its_oldest_events_and_counts_what_it_gave_up() {
        let bus = Bus::with_capacity(3);
        let mut watcher = bus.subscribe();
        for index in 1..=5 {
            bus.publish(published(index, &format!("t{index}")));
        }

        let (events, dropped) = watcher.drain();
        assert_eq!(
            titles(&events),
            ["t3", "t4", "t5"],
            "the newest three survive and the oldest two are the ones given up; dropping the \
             newest instead would hide the current state of the run behind its oldest lines"
        );
        assert_eq!(
            dropped, 2,
            "the view is told exactly how much of the run it did not get to show"
        );

        let (second, second_dropped) = watcher.drain();
        assert!(
            second.is_empty() && second_dropped == 0,
            "the count covers the window it was reported with and does not climb again on the \
             next read: {second_dropped}"
        );
    }

    #[test]
    fn a_ring_sized_zero_holds_nothing_and_counts_everything_dropped() {
        let bus = Bus::with_capacity(0);
        let mut watcher = bus.subscribe();

        bus.publish(published(1, "now you see it"));
        bus.publish(published(2, "now you don't"));

        let (events, dropped) = watcher.drain();
        assert!(
            events.is_empty(),
            "a ring asked to hold nothing holds nothing, rather than panicking on the divide, \
             refusing the publish, or quietly growing: {events:?}"
        );
        assert_eq!(
            dropped, 2,
            "both events were given up and the interface is told both, rather than being shown \
             a silence it would read as a run that stopped producing output"
        );
    }

    #[test]
    fn a_default_bus_is_sized_by_the_ring_the_design_documents() {
        let ring = Config::default().output_ring_lines;
        let one_more = u64::try_from(ring).expect("a ring size is a count of events") + 1;
        let bus = Bus::new();
        let mut watcher = bus.subscribe();

        for index in 0..one_more {
            bus.publish(published(index + 1, &format!("t{index}")));
        }

        let (events, dropped) = watcher.drain();
        assert_eq!(
            u64::try_from(events.len()).expect("a number of events fits in a sequence number"),
            one_more - 1,
            "one event less than was published survives: the ring is `output_ring_lines` deep, \
            the size docs/DESIGN.md gives that key, and not a second number written down here"
        );
        assert_eq!(
            dropped, 1,
            "one event more than the ring could hold, and exactly one was given up"
        );
        assert_eq!(
            titles(&events)
                .last()
                .expect("a full ring holds more than one event")
                .as_str(),
            format!("t{ring}"),
            "and what survived is the newest window, not an arbitrary slice of it"
        );
    }

    #[test]
    fn a_publish_with_no_subscribers_keeps_nothing_and_fails_nothing() {
        let bus = Bus::with_capacity(4);

        bus.publish(published(1, "nobody was watching"));

        let mut late = bus.subscribe();
        assert!(
            late.drain().0.is_empty(),
            "an event that reached nobody is not queued up for whoever looks next: the journal \
             holds it, and a ring that kept it would be an unbounded buffer wearing a bound"
        );
        assert_eq!(
            slots_held(&bus),
            1,
            "the publish left nothing behind; the one slot is the view opened just now"
        );
    }

    #[test]
    fn a_subscriber_that_never_reads_neither_stalls_the_publisher_nor_grows_the_ring() {
        const PUBLISHED: u64 = 20_000;
        let capacity = 8_usize;
        let deep = u64::try_from(capacity).expect("a ring capacity is a small number");
        let bus = Bus::with_capacity(capacity);
        let mut never_read = bus.subscribe();

        // The loop runs on another thread and hands the bus back when it finishes, so
        // a publisher that waits for its reader fails this test at the deadline
        // instead of hanging the suite — and the bus can still be measured after.
        let (sent, received) = mpsc::channel();
        std::thread::spawn(move || {
            for index in 0..PUBLISHED {
                bus.publish(published(index + 1, &format!("t{index}")));
            }
            sent.send(bus)
                .expect("the thread that opened the view is still listening");
        });

        let bus = received.recv_timeout(PUBLISH_DEADLINE).expect(
            "twenty thousand events were published into a ring nobody ever read, so \
                     publishing never blocked on the subscriber",
        );
        let (kept, dropped) = never_read.drain();

        assert_eq!(
            u64::try_from(kept.len()).expect("a ring's contents are countable"),
            deep,
            "the ring still holds exactly its capacity after twenty thousand publishes, which \
             is what makes the memory a run costs a fact about its configuration rather than a \
             fact about how often anyone looked at it"
        );
        assert_eq!(
            titles(&kept)
                .last()
                .expect("a full ring holds an event")
                .as_str(),
            format!("t{}", PUBLISHED - 1),
            "and the newest event is among them: the run did not stall, it outgrew the view"
        );
        assert_eq!(
            u64::try_from(dropped).expect("a number of dropped events fits in a sequence number"),
            PUBLISHED - deep,
            "every event the ring gave up is counted, so a view that was not reading says so \
             rather than appearing to have watched the whole run"
        );
        assert_eq!(
            slots_held(&bus),
            1,
            "and the bus itself holds one slot for one subscriber: a publish adds nothing to it, \
             so nothing accumulates behind a reader that stopped reading"
        );
    }

    #[test]
    fn a_subscriber_that_went_away_is_pruned_by_the_next_publish() {
        let bus = Bus::with_capacity(4);
        let mut watcher = bus.subscribe();
        let closing = bus.subscribe();
        bus.publish(published(1, "both were here"));

        drop(closing);
        assert_eq!(
            slots_held(&bus),
            2,
            "nothing has been published since the view closed, so the slot it left is still \
             there: pruning is something a publish does"
        );

        bus.publish(published(2, "one was here"));

        assert_eq!(
            slots_held(&bus),
            1,
            "the ring of a subscription that no longer exists goes the first time someone pays \
             for it, so a frontend that opened and closed a view a thousand times over a long \
             run leaves one slot behind rather than a thousand"
        );
        let (events, dropped) = watcher.drain();
        assert_eq!(
            titles(&events),
            ["both were here", "one was here"],
            "the view that stayed is unaffected by the one that left"
        );
        assert_eq!(dropped, 0, "and it lost nothing on account of it");

        drop(watcher);
        bus.publish(published(3, "nobody was here"));
        assert_eq!(
            slots_held(&bus),
            0,
            "with every view closed the bus holds nothing at all, and publishing into that is a \
             no-op rather than an error"
        );
    }

    #[test]
    fn a_subscriber_that_panics_holding_its_ring_does_not_wedge_the_bus() {
        let bus = Bus::with_capacity(4);
        let mut watcher = bus.subscribe();
        bus.publish(published(1, "before the panic"));

        let ring = Arc::clone(&watcher.ring);
        let died = std::thread::spawn(move || {
            let _held = ring
                .lock()
                .expect("this ring is not poisoned until the line below");
            panic!("a subscriber that dies while holding its own ring");
        })
        .join();
        assert!(
            died.is_err(),
            "the thread above really did panic, which is what poisons the mutex: {died:?}"
        );

        bus.publish(published(2, "after the panic"));

        let (events, dropped) = watcher.drain();
        assert_eq!(
            titles(&events),
            ["before the panic", "after the panic"],
            "a panic in one subscriber's own code leaves the bus able to publish and this view \
             able to read: another thread's bug is not this supervisor's outage"
        );
        assert_eq!(dropped, 0, "and nothing was lost but the panicking thread");
    }

    #[test]
    fn the_debug_view_of_a_bus_counts_rather_than_prints_the_run() {
        let bus = Bus::with_capacity(4);
        let mut watcher = bus.subscribe();
        bus.publish(published(1, "a line nobody needs printed twice"));

        let bus_text = format!("{bus:?}");
        let watcher_text = format!("{watcher:?}");

        assert!(
            bus_text.contains("capacity") && bus_text.contains("slots"),
            "a bus's debug view says how deep its rings are and how many views are open: \
             {bus_text}"
        );
        assert!(
            !bus_text.contains("printed twice"),
            "and does not dump the events, which over a real run is `output_ring_lines` of \
             agent output per subscriber: {bus_text}"
        );
        assert!(
            watcher_text.contains("unread: 1"),
            "a subscription's debug view counts what is unread: {watcher_text}"
        );
        assert!(
            !watcher_text.contains("printed twice"),
            "without printing what it is unread: {watcher_text}"
        );

        let _ = watcher.drain();
    }

    #[test]
    fn a_record_is_written_to_the_journal_and_the_written_record_is_what_is_published() {
        let (_parent, journal) = a_journal();
        let mut recorder = Recorder::new(journal);
        let mut watcher = recorder.subscribe();

        recorder
            .record(
                Some(TaskId::new(4)),
                EventKind::TaskQueued {
                    title: "record it, then show it".to_owned(),
                },
            )
            .expect("an event appends, and is then published");

        let (published, dropped) = watcher.drain();
        let held = recorder.journal.events().expect("the journal reads back");

        assert_eq!(published.len(), 1, "the view that was open was told");
        assert_eq!(dropped, 0, "to a ring that was nowhere near full");
        assert_eq!(
            held.len(),
            1,
            "one record in the file for one call: the bus is not a second place an event is \
             written, and a recorder that wrote two would have a replay and a screen disagree"
        );
        assert_eq!(
            published[0], held[0],
            "what the bus carried is the stored envelope — the sequence the database issued and \
             the instant the append stamped — rather than a copy of what this call meant to \
             write: {:?} against {:?}",
            published[0], held[0]
        );
        assert_eq!(
            held[0].task_id,
            Some(TaskId::new(4)),
            "attributed to the task the call named"
        );
    }

    #[test]
    fn a_record_answers_with_the_sequence_the_journal_gave_the_record() {
        let (_parent, journal) = a_journal();
        let mut recorder = Recorder::new(journal);
        let mut watcher = recorder.subscribe();

        let mut sequences = Vec::new();
        for _ in 0..3 {
            sequences.push(
                recorder
                    .record(None, EventKind::PreflightStarted)
                    .expect("each event appends and is published"),
            );
        }

        assert_eq!(
            sequences,
            [EventSeq::new(1), EventSeq::new(2), EventSeq::new(3)],
            "the answer is the journal's own number for the record, not a tally of this \
             recorder's calls: {sequences:?}"
        );
        assert_eq!(
            published_sequences(&watcher.drain().0),
            [1, 2, 3],
            "and every view is told the numbers the recorder was given, so a screen can ask the \
             journal about a line it is already showing"
        );
    }

    #[test]
    fn an_event_about_the_queue_itself_is_published_without_a_task() {
        let (_parent, journal) = a_journal();
        let mut recorder = Recorder::new(journal);
        let mut watcher = recorder.subscribe();

        recorder
            .record(None, EventKind::PreflightStarted)
            .expect("an event about the queue itself appends and is published");

        let (published, _) = watcher.drain();
        assert_eq!(
            published[0].task_id, None,
            "the `NULL` of the schema's `task_id` column reaches the frontend, so a queue-level \
             line is not attributed to task 0: {:?}",
            published[0].task_id
        );
        assert_eq!(
            recorder.journal.events().expect("the journal reads back")[0].task_id,
            None,
            "and it is stored that way rather than invented on the way out"
        );
    }

    #[test]
    fn a_subscriber_that_never_reads_cannot_stop_a_record() {
        let (_parent, journal) = a_journal();
        let mut recorder = Recorder::with_bus(journal, Bus::with_capacity(4));
        let mut never_read = recorder.subscribe();

        for _ in 0..50 {
            recorder
                .record(Some(TaskId::new(1)), EventKind::Resumed)
                .expect("every record still appends, with a view that will not read");
        }

        let (kept, dropped) = never_read.drain();
        assert_eq!(
            recorder
                .journal
                .events()
                .expect("the journal reads back")
                .len(),
            50,
            "not one record was lost to a frontend that stopped reading: the durable half of a \
             run does not depend on the display half"
        );
        assert_eq!(kept.len(), 4, "the view keeps the window it was sized for");
        assert_eq!(
            dropped, 46,
            "and is told the 46 it missed, so the screen can say it is behind instead of \
             looking current"
        );
    }

    #[test]
    fn a_record_prunes_a_view_that_went_away() {
        let (_parent, journal) = a_journal();
        let mut recorder = Recorder::with_bus(journal, Bus::with_capacity(4));
        let closing = recorder.subscribe();
        assert_eq!(slots_held(&recorder.bus), 1, "one view is open");

        drop(closing);
        recorder
            .record(None, EventKind::PreflightStarted)
            .expect("a record with no views open still appends");

        assert_eq!(
            slots_held(&recorder.bus),
            0,
            "the record found nobody to publish to and left nothing behind, so a run whose \
             frontend came and went costs nothing for the visits it stopped making"
        );
    }

    /// The append is refused by the file rather than by a mock of it: a second
    /// connection adds a `BEFORE INSERT` trigger that raises, which is the same
    /// mechanism ADR-0017 uses for the append-only guards.
    ///
    /// A trigger rather than a held write lock because a contended lock makes the
    /// journal wait out SQLite's five-second busy timeout before it is refused, and
    /// what this test is about is what a recorder publishes when a record is
    /// refused — not how long the journal waits before refusing one.
    #[test]
    fn a_record_the_journal_refuses_reaches_no_subscriber() {
        let parent = tempdir().expect("a scratch directory below the system temp directory");
        let path = parent.path().join("journal.db");
        let journal = Journal::open(&path).expect("a new journal opens");
        let mut recorder = Recorder::with_bus(journal, Bus::with_capacity(8));
        let mut watcher = recorder.subscribe();

        let other = rusqlite::Connection::open(&path).expect("a second connection to the file");
        other
            .execute_batch(
                "CREATE TRIGGER test_refuses_insert BEFORE INSERT ON events BEGIN \
                 SELECT RAISE(ABORT, 'a test refuses every insert'); END;",
            )
            .expect("the journal's file accepts a trigger");

        let refused = recorder
            .record(Some(TaskId::new(1)), EventKind::Resumed)
            .expect_err("a journal that cannot be written refuses the record");
        assert!(
            matches!(refused, Error::Database(_)),
            "the refusal is the database's own, handed over rather than wrapped in a \
             fabrication: {refused}"
        );

        let (events, dropped) = watcher.drain();
        assert!(
            events.is_empty(),
            "nothing was journaled, so nothing was published: a frontend shown a transition \
             that no replay contains is the exact failure this type exists to prevent: {events:?}"
        );
        assert_eq!(
            dropped, 0,
            "a refusal is not a lost event, and a view that is told it lost one would look for \
             a record that was never written"
        );
        assert!(
            recorder
                .journal
                .events()
                .expect("the journal reads back")
                .is_empty(),
            "and the refusal wrote nothing, which is why there was nothing to publish"
        );

        other
            .execute_batch("DROP TRIGGER test_refuses_insert")
            .expect("the refusal is taken away again");
        let written = recorder
            .record(Some(TaskId::new(1)), EventKind::Resumed)
            .expect("a refused record does not wedge the recorder");
        assert_eq!(
            written.get(),
            1,
            "the refusal spent no sequence: the next record is the journal's first, so a \
             refused record leaves nothing behind in the file either"
        );
        assert_eq!(
            published_sequences(&watcher.drain().0),
            [1],
            "and the record after the refusal reaches the view that waited through it, so one \
             refused write costs the run the event it could not write and nothing else"
        );
    }

    #[test]
    fn a_record_the_journal_cannot_hand_back_is_refused_naming_its_sequence() {
        let (_parent, journal) = a_journal();

        let refused = stored_event(&journal, EventSeq::new(4_242))
            .expect_err("a sequence with no record behind it is not a record");

        assert!(
            matches!(
                refused,
                Error::Corrupt {
                    seq: Some(4_242),
                    ..
                }
            ),
            "the refusal is damage, reported against the sequence that was looked for: {refused}"
        );
        assert!(
            refused.to_string().contains("4242"),
            "the message carries the number, because whoever reads it has to know which record \
             was expected: {refused}"
        );

        let mut recorder = Recorder::new(journal);
        let written = recorder
            .record(Some(TaskId::new(2)), EventKind::Resumed)
            .expect("an event appends");
        let read = stored_event(&recorder.journal, written)
            .expect("the record just written is readable by the sequence it was written at");

        assert_eq!(
            read.kind,
            EventKind::Resumed,
            "the same helper that refuses a number with no record behind it hands back the \
             record for a number that has one: {:?}",
            read.kind
        );
    }
}

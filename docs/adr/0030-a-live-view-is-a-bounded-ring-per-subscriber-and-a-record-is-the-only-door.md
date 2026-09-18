# 0030. A live view is a bounded ring per subscriber, and a record is the only door

- **Status:** accepted
- **Date:** 2026-09-17

## Context

VISION.md section 5 keeps terminal I/O out of `ktask-core`: "progress and events
flow through typed channels/callbacks", with both frontends consuming the same
stream so everything the TUI shows stays scriptable from the CLI. What that
sentence asks for is a concurrency decision, and the two consumers pull against
each other. The TUI redraws at frame rate and can show no more than
`output_ring_lines` of output; a CLI printing each record once is as slow as a
pipe nobody is reading. Neither may hold the other up, and neither may hold the
*run* up — VISION.md section 3 journals every transition before the side effect
it describes, so the cost of recording an event has to stay a fact about the
journal rather than become a fact about a screen.

Two failure modes are therefore unacceptable, and they are the whole design
problem: publishing waits for a reader (a hung frontend becomes a hung
supervisor), or the publisher buffers for a reader that never reads (the run's
memory becomes a function of nobody looking at it). `docs/DESIGN.md` fixes the
shape that avoids both — a bounded ring per subscriber, capacity
`output_ring_lines`, drop-oldest, with a `dropped: usize` the interface can
display — leaving the mechanism to choose.

The second half of the task is not concurrency. "Nothing is done on an agent's
say-so" means what a frontend displays must be something the durable record also
says, and ADR-0016 gave the sequence and the instant to the journal. So the
question is not only how an event reaches a screen, but how an event comes to
exist at all, and whether a screen can be shown one that a replay does not
contain.

## Decision

**A ring per subscriber, held weakly.** `Bus` keeps one
`Weak<Mutex<Ring>>` slot per view; the `Subscription` itself owns the ring, and
the bus cannot keep a view's buffer alive behind it. A `Ring` is a `VecDeque`
with a capacity, drop-oldest on overflow, and its own `dropped` count, so a view
that fell behind is told exactly what it missed and can say so on screen instead
of looking current. The capacity comes from `Config::default().output_ring_lines`
rather than a second literal, and `Bus::with_capacity` lets a run size rings from
its own configuration file. A capacity of `0` is answered rather than refused:
every event is given up at once and counted.

**Publishing prunes, in the pass that collects.** Each publish upgrades every
slot once and keeps the upgraded ones, so a slot whose subscriber has been
dropped is gone the first time anybody pays for it, and a live view cannot slip
between a liveness check and the push because there is one pass under one lock.

**A `Bus` clones; a `Subscription` does not.** Cloning a bus shares the slot
list and adds no subscriber — it is a handle for whoever needs to publish or
subscribe later. A cloned subscription would be a second reader of one ring,
silently dividing its events between the two, so there is no `Clone`: two readers
take two subscriptions and each gets every event.

**A record is the only way an event comes to exist.** `Recorder { journal, bus }`
appends and then publishes; nothing else in the crate's non-test code calls
`Journal::append` or `Bus::publish`, and a refusal from the journal publishes
nothing, because nothing happened. What is published is the *stored* envelope,
read back by its sequence — not an envelope the recorder built.

**`Journal::event` is the read that makes that affordable.** An equality read on
the primary key: one row per recorded event. `events_since(seq - 1)` would decode
the tail of the journal for every published event, and would let damage in a row
nobody asked about refuse a record that decoded correctly. It answers `Ok(None)`
for a sequence with no row — a sequence an interrupted commit spent is a real
thing to ask about (ADR-0016) — and it refuses to clamp a too-wide `u64` into the
signed column the way the cursor reads do, because an equality read against the
clamp would answer with whatever row sits at the clamp and call it the one that
was asked for.

**Poison is recovered, not propagated.** Every lock here is taken with
`unwrap_or_else(PoisonError::into_inner)`. No code a caller owns runs under these
locks, so poison means a bug inside this module; the outage that matters is the
supervisor's, and a bus that stops publishing takes every frontend down with it.

No dependency is added: `rusqlite`, `serde` and `time` were already required, so
`Cargo.lock` does not change.

## Alternatives considered

- **One shared ring with a cursor per subscriber.** Rejected: to stay bounded a
  shared ring must drop its oldest event, and the oldest event is precisely the
  one some other subscriber has not read yet. A frontend that stops reading would
  cost every other frontend its events. Per-subscriber rings cost N copies —
  ADR-0020 measured `size_of::<Event>()` at 88 bytes, so a full 4 096-deep ring is
  about 352 KiB per view, which is bounded, knowable, and shrinks with the
  configuration key that sizes it.
- **`tokio::sync::broadcast`.** It reports lagged readers, which is tempting, and
  it drags an async runtime into a crate the design keeps synchronous. The
  dependency set is fixed by `docs/DESIGN.md` and a new crate is its own ADR;
  nothing here needs a reactor.
- **`std::sync::mpmc`.** One message reaches one receiver: that is a work queue,
  not a broadcast, and both frontends must see every event. Giving each reader its
  own channel is the fan-out this task is asked to write, written worse — the
  bound becomes per-channel and there is nowhere honest to keep a drop count.
- **A counter kept beside the ring rather than inside it.** The bound is
  per-subscriber, so the count of what that subscriber lost has to live with the
  buffer that lost it; a bus-wide counter would attribute one view's backlog to
  every view.
- **Publishing the caller's `EventKind` in an envelope the recorder builds.**
  Saves one indexed read per record and costs the truth of the record. The
  sequence is the database's `AUTOINCREMENT` counter and the instant is stamped
  inside the append (ADR-0016); a recorder that guessed at either would put a
  screen and a replay an unknown number of nanoseconds apart, and the sequence it
  guessed at is one interrupted commit away from naming a different record.
  `Journal::event` is a primary-key read, so the honesty costs one row of work.
- **Making `Journal::append` `pub(crate)`,** which would make "the recorder is the
  only door" a compile-time fact. Rejected: the crash-recovery harness
  `crates/ktask-core/tests/durability.rs` has to call the append directly to
  control the instant a process is killed inside it, and it is an external test
  crate. The rule is held by the module's shape instead — `Bus::publish` has one
  caller, `Recorder::record` — which is weaker than the compiler and is recorded
  here so the next person looks rather than assumes.

## Consequences

A view sees the present and never the past: a frontend that arrives mid-run gets
nothing until the next event and reads history from the journal
(`Journal::events_since`). That is the intended division — the journal answers
"what happened", a `Subscription` answers "what just happened" — and it means the
bus is not a cache, a queue, or a place a run can find out what it did.

Memory for a live run is now a product of configuration: capacity × subscribers,
plus whatever each event's payload strings own. Nothing about a reader's behaviour
changes it. `dropped` covers the window since the last `drain`, so a screen
polling each frame can print "N earlier events lost" beside what it did show.
`Debug` for both `Bus` and `Subscription` prints counts, never the events, so the
largest thing in a panic report is not the output view someone was debugging.

Recording now costs one extra indexed read per event. If that ever shows in a
profile, the honest fix is for `Journal::append` to hand back the row it wrote —
one row shape, one decode, no second copy — and not for the recorder to build its
own envelope; that is a change to the journal's API and belongs to whichever task
is forced to make it.

A second `ktask` process watching the same project sees none of this: the bus
lives and dies with its process. A run started elsewhere is followed by polling
the journal by cursor, which is what that poll's task is for.

One line here is unkillable by a test and is named rather than hidden: the
explicit `drop(kind)` in `Recorder::record`. The parameter is by value because the
task fixes that signature, and the statement says out loud that the caller's copy
is spent by the append. Deleting it changes no behaviour; suppressing the
resulting lint with `#[allow]` was not an option worth taking, because a
line-level suppression is exactly the weakening this project refuses.

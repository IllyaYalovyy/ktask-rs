# 0001. `run_gate` captures output but publishes none of it to the bus

- **Status:** accepted
- **Date:** 2026-09-22

## Context

T041 defines `run_gate(gate, root, bus)` and asks it to "publish output
chunks to the bus as they arrive using reader threads." Three parts of that
are mechanics this task owns outright: spawn the command, read both pipes on
their own threads so neither can stall the other, enforce the timeout. The
fourth — publishing to `bus` — runs into constraints the repository already
states elsewhere:

1. **The event catalog has no payload for a chunk of gate output.**
   `event.rs`'s module doc is explicit that `GateStarted` and `GateFinished`
   are "deliberately absent because their payload names a type no earlier
   task has defined," and that each "is added by the task that defines its
   payload type, which also adds its arm to the transition function."
   `docs/DESIGN.md` calls the event table exhaustive: "nothing else may be
   added without extending `state::apply` in the same task." There is no
   variant today that can hold a stream tag and a line of text, let alone one
   attributed to a task and attempt.
2. **`Bus::publish` is documented as `Recorder`'s alone to call.**
   `events.rs`'s module doc: "`Recorder` is the only thing allowed to call
   `Journal::append` or `Bus::publish`: appending to the journal and
   publishing the same event happen together, so nothing can reach a
   frontend that is not already durable." Publishing straight from
   `run_gate` would mean a screen showing something a crash-and-replay could
   never reproduce — the exact failure mode that module exists to prevent.
3. **`run_gate` cannot construct the envelope even if the payload existed.**
   Publishing means building an `Event { seq, ts, task_id, kind }`.
   `run_gate` is not given a task id, and `seq` belongs to the journal
   (`Recorder::record` reads it back from the append rather than choosing
   it) — a sequence number this function invented would collide with, or
   race, whatever the journal assigns next.

## Decision

`run_gate` accepts `bus: Option<&Bus>` per its required signature and does
not call `Bus::publish` through it. The streaming mechanics the task asked
for are real: both pipes are read on dedicated threads, chunks are collected
as they arrive (not batched until exit), and a gate that times out still
returns whatever output had landed by then. What is missing is only the
last step — handing a chunk to `bus` — because there is nowhere honest to
put it yet.

A test (`gate::tests::run_gate::a_bus_is_accepted_but_nothing_is_published_to_it_yet`)
pins this down: a subscriber attached to the bus passed into `run_gate` sees
zero events. When a later task adds a catalog entry for gate output and
threads a task/attempt id through, wiring it in is a change to what happens
inside the collector's `Ok(chunk) => ...` arm — not a signature change, and
not a change to any caller written against today's signature.

## Alternatives considered

- **Add a `GateOutput { stream, text }` variant now and record it.**
  Rejected: this task's files are scoped to `gate.rs`; adding a catalog
  entry requires an `apply` arm in `state.rs` "in the same task" per
  `docs/DESIGN.md`, and deciding whether gate output is journaled at all
  (a `cargo test` run can be thousands of lines) is a durable-data decision,
  not a mechanics one.
- **Give `Bus` a second, non-journaled publish path for ephemeral chunks.**
  Rejected: it reverses the guarantee `events.rs`'s doc states today — that
  nothing reaches a subscriber that was not already durable — for exactly
  the highest-volume output source the supervisor has.
- **Drop the `bus` parameter until it can be honoured.** Rejected: T041
  names the signature; a caller wiring a real bus in later should not need
  to change `run_gate`'s call site, only what happens once the chunk is in
  hand.

## Consequences

A live viewer still learns everything a gate produced, just not while it
runs: `GateResult::stdout` / `GateResult::stderr` hold the full transcript,
and `docs/CONTRACT.md`'s promise of "gate results as they land" is a
finished result, not a byte at a time. True mid-run streaming to a pane
still needs the catalog entry and the id-threading this ADR did not invent;
until that task lands, passing a bus to `run_gate` changes nothing an
observer can see. Whoever does that work should start from the collector
loop in `run_gate`, not rewrite it.

# 0037. A gate's output is captured here and published nowhere until the catalog has a home for it

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T041 defines `run_gate(gate, root, bus)` in `crates/ktask-core/src/gate.rs` and
asks for four things: spawn the command, capture both streams, "publish output
chunks to the bus as they arrive using reader threads", and enforce the timeout.
Three of those are mechanics, and this ADR does not decide them. The third is a
durable-data decision wearing a mechanics task, and the repository already
answers it.

What was checked rather than assumed, at the commit this landed on:

1. **The catalog cannot carry a chunk.** `docs/DESIGN.md:214` states that the
   listed payloads are exhaustive and "nothing else may be added without
   extending `state::apply` in the same task". `EventKind` has no variant that
   holds a piece of command output; the two gate entries that do exist in the
   table, `GateStarted` and `GateFinished`, are in `event.rs`'s
   `DEFERRED_PAYLOADS` (crates/ktask-core/src/event.rs:368) and a test asserts
   each is *refused* on decode. ADR-0011 records why that refusal is the point:
   an entry with no producer and no `apply` arm is a record whose effect on
   state nobody specified, which is the one artifact replay cannot reproduce.
   ADR-0022 prices the arm — one exhaustive match in each of eight per-state
   helpers, plus the terminal refusals.
2. **The bus has one door, and it is not this function's.** `Bus::publish` is
   `pub` (crates/ktask-core/src/events.rs:174), but its own doc says
   "`Recorder` is what publishes, and it is the only thing that does"; outside
   `Recorder::record` (crates/ktask-core/src/events.rs:324) the only callers are
   tests in that module. ADR-0030 chose that on purpose: a live view is a replay
   of what was recorded, so an event a screen saw that a replay could not
   reproduce is the failure mode `events` exists to prevent.
3. **A chunk is not an envelope.** To publish one, `run_gate` would have to
   construct an `Event`, which means choosing `seq` and `ts` itself. ADR-0016
   forbids exactly that — "a sequence a caller chose is a sequence two callers
   can choose" — and `Recorder` gets both right by reading the stored row back
   after the append. Gate output has no row to read back, because there is no
   entry to append.
4. **The signature cannot attribute what it publishes.** `run_gate` receives a
   gate, a directory and a bus. It is not told which task the gate belongs to.
   VISION.md:270 requires pane output to be "always attributable to a task, a
   phase and a moment in time" — a moment that, per point 3, only the journal
   owns. So even with a catalog entry invented here, the event would be missing
   the one field the pane needs, and `task_id` is part of the durable envelope.
5. **The interface does not actually block.** `docs/CONTRACT.md:175` promises
   the Live run pane "gate results as they land" — a finished `GateResult`, not
   a byte at a time. Streaming *while a gate runs* is VISION.md's output-pane
   requirement, and it is a real requirement; it is simply not satisfiable
   through this bus without inventing 1–4 above.

## Decision

`run_gate` captures everything and publishes nothing. `bus` is accepted,
documented at the function, and left unconsumed.

- **The streaming is real and tested; only the destination is missing.** All
  three mechanics — two reader threads, one collector, the timeout — live in a
  private `run_gate_streaming(gate, root, on_chunk)`, and `run_gate` is that
  function with a callback that discards. Chunks arrive line-by-line on the
  caller's thread, in the order they were observed, tagged with their stream.
  Tests assert arrival *timing* (a line lands while the gate is still sleeping,
  its successor lands a second later), stream attribution, a final line with no
  newline, and non-UTF-8 bytes arriving replaced rather than dropped.
- **One line stands between this and a live pane.** Whoever lands the catalog
  entry passes a callback that publishes instead of one that discards; nothing
  about the threads, the ordering, or the timeout changes. `run_gate`'s
  public signature keeps the `bus` parameter, so the caller written today is
  the caller that will work then.
- **Output still reaches a viewer, in the form the contract promises.** A
  finished run returns `stdout`/`stderr` whole inside its `GateResult`, and
  that is the record the pane is documented to show "as they land".

## Alternatives considered

- **Add a 29th catalog entry (`GateOutput { stream, text }`) and record it.**
  Rejected: it decides the journal's durable format for a byte-at-a-time stream
  nobody sized, and every chunk would be a row — a `cargo test` run is thousands
  of appends inside one gate, each taking the write lock, each needing an
  `apply` arm (ADR-0022) whose only effect is "append text to a pane". Batching
  would soften the cost and not the missing `apply` semantics, and
  `docs/DESIGN.md:214` exists to stop precisely an entry arriving ahead of its
  producer. This is a decision about durable data and the shape of `apply`, so
  it is reported as a finding for a decision-maker, not made here.
- **Give `Bus` a second, ephemeral door for non-journaled chunks.** Rejected:
  it is ADR-0030's accepted decision reversed, and reversing it is not this
  task's authority. It also has a cost the ADR named: a subscriber that missed
  the chunk (ring overflow, or a view opened after the gate ran) has no way to
  recover it, because nothing was written down. Gate output is the largest
  producer of such chunks the supervisor will have.
- **Publish through the `Recorder`, appending each chunk as `GateFinished`-
  adjacent data.** Rejected: it appends rows whose `apply` arms do not exist,
  which turns point 1 from a documented debt into a journaled lie.
- **Drop the `bus` parameter until it can be honoured.** Rejected: T041 names
  the signature, and the caller that hands a gate a live view is written by the
  orchestration task (T085), not by this one. A parameter that is inert is a
  visible debt; a missing one is a signature change and a churned call site.
- **Publish a truncated or rate-limited preview.** Rejected: it is the first
  option's blocking objection with less data behind it, and a pane that shows a
  lie about a gate's output is worse than one that shows the finished result.

## Consequences

Four of the mechanics a live pane needs are now finished and pinned by tests:
capture that survives a kill, stream attribution, bounded arrival, and
replacement of bytes that are not text. What remains is a decision, not work in
this file.

The debt is visible in three places, which is the point: the parameter is inert
at the call site, the reason is a doc comment above it, and this ADR is the one
door back to the options. `gate.rs` carries a test
(`a_listening_bus_changes_nothing_about_what_a_gate_reports`) that asserts a
live subscriber sees *zero* events and that nothing was dropped — so when a
later task publishes gate output, that test fails loudly and its author must
decide what the assertion should become, rather than having the behavior change
under a green suite.

A later decision here must still answer what this one could not: which catalog
entry carries a chunk, how often one is recorded (per line? per
kilobyte? per poll?), what `state::apply` does with it, how a pane rebuilds
output after a restart, and which task a chunk belongs to.

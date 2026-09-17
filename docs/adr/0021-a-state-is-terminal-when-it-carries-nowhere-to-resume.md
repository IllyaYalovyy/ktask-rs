# 0021. A state is terminal when it carries nowhere to resume

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T021 adds `TaskState` with `is_terminal`, `is_paused` and `name`. The
`docs/DESIGN.md` Core types section fixes the twelve variants and their
payloads exactly, and it says nothing about which of them are terminal — the
predicate is named by the task and defined by nobody. That has to be settled
here rather than left open, because `state::apply` reports an illegal
transition *from* one of these states, the queue drain decides whether a
predecessor has finished, and the interface colours a task that needs a human;
all three read this predicate, and two different answers are defensible from
the documents.

What the documents do say pulls in both directions:

- `docs/CONTRACT.md` `run`: "Stops at the first **terminal failure**" — so a
  `Failed` task is a stopping point, which argues `Failed` is terminal.
- `docs/CONTRACT.md` `retry`: "Starts a **fresh** remediation attempt seeded
  with the failure bundle" — so something leaves `Failed`, which argues it is
  not.
- `ktask-rs ack` passes a human gate and "leaves the queue paused", and
  `GateAcknowledged { by, at }` is the event that produces
  `Acknowledged { by, at }` — so a state named for a human act has a producer,
  and `Done` can be where that event is applied.
- `TaskState::Paused` is the one variant that stores `resume_to: Box<TaskState>`
  — the type itself says where it goes next.

## Decision

`is_terminal` is true for exactly `Done`, `Acknowledged`, `Failed` and
`Cancelled`; `is_paused` is true for exactly `Paused`, whatever it boxes. The
rule behind the list is structural rather than a taste: **a state is terminal
when the type names no state to return to, so nothing the supervisor's own loop
emits moves it on.** Every way out of the four is a human command that starts
new work or records a human act — `retry` seeds a fresh attempt rather than
continuing the failed one, `ack` records a decision, `cancel` is what let the
queue proceed. `Paused` is the contrast case, and stays non-terminal even when
its `resume_to` is itself terminal: it is waiting, not finished.

`name` returns the variant's own name as `docs/DESIGN.md` spells it, which is
the identity of the state and not a rendering of it; `state::apply` hands it to
`Error::InvalidTransition { from }`. The lowercase, hyphenated forms `--json`
output prints belong to whoever is rendering, so they are not defined here.

## Alternatives considered

- **Terminal = `Acknowledged` and `Cancelled` only**, on the grounds that
  `retry` leaves `Failed`. Rejected: it makes the stopping point `run` is
  documented to stop at unnameable, and it asks a predicate over *states* to
  know about `retry`, which is a command.
- **Terminal = `Done`, `Failed`, `Cancelled`**, excluding `Acknowledged`
  because `ack` continues the queue. Rejected: `ack` "leaves the queue paused"
  and `resume` continues it, so what continues is the pause, and the rule above
  already excludes the human act by its own terms. Including `Done` while
  excluding the state that records its acknowledgement is the worse drift.
- **Derive terminality from a `Phase` or an attempt count** rather than the
  variant. Rejected: it needs the machine's history to answer a question the
  type already answers, and `apply` is pure.

## Consequences

- `is_terminal` and `is_paused` are disjoint by construction and a test pins
  that no state is both, which is what lets a screen ask either question —
  "is this finished" and "is this waiting on someone" — without a third
  predicate to keep them apart.
- `is_terminal` is *not* "the run stops here": a drain walks past `Done` and
  stops at a `Failed` task, and that difference belongs to the drain, not to
  this predicate. A caller that wants "may the queue proceed" combines it with
  the state's payload.
- The task that writes `apply` must let exactly the human-issued events reach
  or leave these four states. If it turns out to need a machine-generated event
  leaving one of them, that is the assumption that has changed, and this ADR is
  what to revisit.

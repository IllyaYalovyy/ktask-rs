# 0001. Error payloads for not-yet-defined types use their underlying form

- **Status:** accepted
- **Date:** 2026-09-17

## Context

`docs/DESIGN.md` fixes the `Error` enum, and two of its payloads name types
that do not exist at the point the error type must exist:

- `Error::Gate { kind: GateKind, .. }` — `GateKind` is defined by T038 in
  `gate.rs`.
- `Error::Corrupt { seq: Option<EventSeq>, .. }` — `EventSeq` is defined by
  T002 in `ids.rs`.

`Error` is T001: every other task is written against it, so it cannot wait for
the tasks that supply those names. Nor can T001 define them early. Defining
`EventSeq` in `error.rs` collides with T002, whose declared files are `ids.rs`
and `lib.rs`; the same collision applies to `GateKind` and T038. Defining a
type twice, or moving one between crates' modules from a task that does not own
it, trades a real conflict for a cosmetic one.

## Decision

`Error::Gate::kind` is a `String` and `Error::Corrupt::seq` is `Option<u64>` —
the representation `EventSeq` wraps. Field names are exactly the ones
`docs/DESIGN.md` lists, so the tasks that own those types narrow the payloads
without renaming a field or rewriting a match arm. Constructing an `Error::Gate`
from a `GateKind` takes `kind.to_string()`; constructing `Error::Corrupt` from
an `EventSeq` takes `seq.get()`.

## Alternatives considered

- **Define `GateKind` and `EventSeq` in T001.** Rejected: both tasks that own
  them declare a file list that does not include `error.rs`, so they would have
  to delete a duplicate outside their scope.
- **Let T001 wait for T002 and T038.** Rejected: 160 tasks name `Error` in
  their signatures; the dependency order runs the other way.
- **Report NEEDS_INPUT.** Rejected: the resolution is mechanical and reversible,
  and no product decision is being made.

## Consequences

Message formatting for a gate failure goes through a `String` rather than
`GateKind`'s own `Display` until T038 lands; the `String` is what `GateKind`
will render as, so no message changes meaning. If T002 or T038 narrows these
payloads, the change is confined to `error.rs` and the construction sites, and
the tests in `error.rs` pin the message format across that change.

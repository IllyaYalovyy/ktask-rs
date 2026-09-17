# 0012. The event envelope formats its own instant rather than widening `time`

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T013 adds the record around a catalog entry — `Event { seq, ts, task_id, kind }`
— and asks that `ts` be "serialized as RFC 3339 in UTC using `time`".
`docs/DESIGN.md` says the same thing twice: its Conventions are "Time is
`OffsetDateTime` in UTC, serialized as RFC 3339", and the `events` table comment
is `ts TEXT NOT NULL, -- RFC 3339, UTC`. So the requirement is not a display
preference. `ts` is durable text an operator greps and a query compares.

Three things make it a decision rather than one attribute.

1. **`time`'s own `serde` encoding is not that text.** With the features
   `docs/DESIGN.md` fixes — `formatting`, `parsing`, `macros`, `serde`, and
   deliberately not `serde-human-readable` — an `OffsetDateTime` encodes as a
   numeric tuple. Measured on this crate at this commit, through the one field
   that still uses the derive (`GateAcknowledged::at`):
   `{"kind":"GateAcknowledged","by":"operators.name","at":[2026,260,12,34,56,0,0,0,0]}`.
   T011 recorded that and handed the choice to whichever task renders an
   instant: widen the workspace `time` features — an edit to the dependency set
   the design fixes, so an ADR of its own — or format at the boundary. T013 is
   the first task to put an instant into a durable record, so T013 is that task.
2. **Writing keeps the offset it was given.** `time::serde::rfc3339` writes
   `2026-09-17T14:34:56+02:00` for the same instant it writes
   `2026-09-17T12:34:56Z`. Delegating to it unchanged would let one column hold
   every local spelling of one instant, which is what the words "in UTC" in the
   column comment exist to rule out. Reading, measured, does not need the same
   help: the RFC 3339 parser hands back the instant it read at the UTC offset
   whatever the text carried.
3. **The obvious conversion panics.** `OffsetDateTime::to_utc` and `to_offset`
   are the natural way to normalize; both are `#[track_caller]` and panic once
   the result leaves the supported date range. This crate promotes `panic`,
   `expect` and `unwrap` to errors on the grounds that a supervisor which
   panics loses the run it was supervising. A serializer is not an exception.

## Decision

`Event::ts` carries `#[serde(with = "rfc3339_utc")]`. The module delegates the
text to `time`'s own well-known-format implementation, `time::serde::rfc3339`,
and adds only what `time` does not do:

- **Write** converts with `checked_to_offset(UtcOffset::UTC)` and turns its
  refusal into a serde error. An hour past the last representable date there is
  no UTC calendar date to write, and `to_offset` panics there.
- **Read** forwards. The parser already yields UTC; a test holds that promise
  so a `time` upgrade that broke it fails here rather than silently storing
  local spellings.

Writes are canonical, reads are canonicalizing but not strict: an RFC 3339 stamp
with an offset, or with a space where the `T` goes, names a real instant, is
read, and leaves again in the one spelling the column documents. A stamp naming
no instant — a number, a bare local time, `time`'s tuple — is refused. The
workspace `time` features, `Cargo.toml` and `Cargo.lock` are unchanged, and
`kind` stays nested rather than flattened, so the payload object inside an
envelope is byte for byte the object the `payload` column will hold.

## Alternatives considered

- **Add `serde-human-readable` to the workspace `time` features.** Viable, and
  where the crate may well end up; declined here because it edits the
  dependency set `docs/DESIGN.md` fixes, changes how every `OffsetDateTime` in
  every crate encodes — including data T011 and T012 already pinned — and still
  would not force UTC. A field-level answer that cannot change anyone else's
  bytes is the smaller commitment, and it survives that later ADR: a `with`
  beats the derive whatever the derive becomes.
- **`#[serde(with = "time::serde::rfc3339")]` directly.** One line, and
  declined on point 2: it preserves the author's offset, so one column holds two
  spellings of one instant and `WHERE ts < '…'` stops ordering by instant.
- **Store `ts` as a `String` or as unix nanoseconds in the struct.** Declined:
  the field type is fixed by the plan, and it is the type that stops every
  consumer inventing its own parse.
- **`to_utc()` / `to_offset()` in the serializer.** Declined on point 3. Both
  ends of the range are pinned instead: `datetime!(9999-12-31 23:30 -01:00)` is
  one hour past the last date, where `checked_to_offset` refuses and the
  unchecked form panics; `datetime!(0000-01-01 00:30 +02:00)` converts to a
  negative year, which RFC 3339 has no digits for, and `time`'s formatter
  refuses it. Two tests, one per end.
- **`#[serde(flatten)]` on `kind`.** Flatter `--json`, declined: the journal
  stores `kind.discriminant()` in one column and the tagged payload object in
  another, so flattening would give one event two shapes. A test asserts the
  envelope's `kind` object *is* `serde_json::to_value(&event.kind)`.
- **Normalizing on read as well.** Written first, then deleted: a hand mutation
  that removed it changed nothing observable, which is the definition of code
  with no reason to exist. The behavior it was defending is asserted instead.
- **`deny_unknown_fields` on `Event`.** Not taken: it would make a journal
  written by a later version unreadable here, and no document asks for it.

## Consequences

- T016 and T018 can write and read the `ts` column as the same text the envelope
  prints, and a row stamped by hand or by an older tool at an offset still
  decodes — to the same instant, in UTC.
- `GateAcknowledged::at` still encodes as the numeric tuple, deliberately. T013
  chose its own field, not the crate's; the task that renders an instant to an
  operator (the History screen, or `--json`) owns that call, and T011's finding
  stays open until then.
- `rfc3339_utc` is private to `event.rs`. When a second field needs it, it moves
  to a shared module in that task rather than being copied: a second copy of a
  timestamp rule is how two timestamp rules come to disagree.

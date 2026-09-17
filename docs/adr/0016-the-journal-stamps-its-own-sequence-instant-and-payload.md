# 0016. The journal stamps its own sequence, instant and payload

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T016 adds `Journal::append(task_id, kind) -> EventSeq`, the only write the
journal makes. It is also where VISION.md section 3's third invariant becomes
mechanical: an event is evidence about a run, and evidence a caller can date and
number is evidence a caller can arrange.

The signature the task fixes supplies *what happened* and nothing else, so three
values in the row have no supplier: `seq`, `ts`, `payload`. Each has a tempting
shortcut — a caller-passed timestamp, `SELECT COALESCE(MAX(seq), 0) + 1`, an
`unwrap` on the encoder — and each shortcut is a fact the journal would stop
owning.

Measured on this toolchain (rusqlite 0.40.2, `serde_json` 1.0.151, `time`
0.3.55), because the done-when asks for a serialization failure and there was no
way to know in advance whether one was reachable:

- `serde_json` refuses a map key that is not a string (`key must be a string`).
  It does **not** refuse a non-finite float: `to_string(&f64::NAN)` is
  `Ok("null")`, and so is `±INFINITY`. The first draft of the failure test used
  NAN, and the append it was testing wrote its row and returned a sequence — the
  refusal had to be measured before it could be asserted.
- No field of `EventKind` is today a value `serde_json` refuses. A `GateAcknowledged`
  holding a negative-year instant is constructible at runtime
  (`from_unix_timestamp_nanos` accepts it) and still encodes, because that field
  uses the derive, whose non-human-readable form is a numeric pair (ADR-0012).
- `time::serde::rfc3339::serialize` is, in 0.3.55, `format(&Rfc3339)` with
  `Error::custom` on refusal. Stamping the column with `format(&Rfc3339)`
  therefore adds no second spelling of an instant.
- `RETURNING seq` reports the number the row was given even when an `AFTER
  INSERT` trigger has since updated it: with a trigger that set `seq = -1`, the
  value handed back was still `1`.
- `rusqlite` 0.40 reads an `INTEGER` as `i8..=i64`, `u8`, `u16`, `u32`, `isize`
  or `usize`, and has no `FromSql` for `u64`.
- `OffsetDateTime::now_utc()` goes through `From<SystemTime>`, whose
  `UNIX_EPOCH + duration` panics when the result is out of range.

## Decision

`append` takes `&mut self`, no sequence and no timestamp, and derives all three
values itself.

`seq` comes from the insert: `INSERT … RETURNING seq`, read as `i64` because
that is the column's signedness, then converted by `event_sequence` **before**
the commit. A number that cannot be a count of events is `Error::Corrupt`, and
because the conversion precedes the commit the insert it came from is rolled
back with it.

`ts` is the clock read inside the call, and formatted by `stamp_text` as RFC
3339 in UTC. The read itself is `clock_nanos`, three lines that decide nothing;
the decisions it feeds are pure and take the reading as an argument —
`reading_nanos` for the side of the epoch and the part below a second,
`stamp_text` for the text and its two refusals. A headless test may not set a
machine's clock, so it hands those functions the reading it wants, which is the
shape `docs/QUALITY.md` asks for.

An instant with no RFC 3339 text is `Error::Serde`: ADR-0012's category,
extended from the `ts` field of an envelope to the column that holds the same
instant. Two refusals are reachable on a machine whose clock has been set
absurdly — a reading outside the instants `time` represents, and one whose year
the format has no digits for — and both are tested.

`payload` is `serde_json`'s encoding of the catalog entry, the same tagged object
`event.rs` defines, and the `kind` column is `kind.discriminant()`. One test
appends a value of all nineteen entries and asserts, for each, that the column
equals the tag inside the payload and that the payload decodes back to the entry
it came from, so neither half can drift alone.

The steps run in a fixed order: encode, stamp, `transaction()`, insert, check
the sequence, commit. A refusal in the first two never reaches the database.

The payload step is handed in through a private `append_encoded`, of which
`append` is a three-line wrapper. Production supplies `serde_json`; the test
supplies a serializer that genuinely refuses. The alternative was a test that
asserted nothing, and the seam is private, so no caller can hand in a payload the
journal did not derive.

No dependency is added: `serde_json`, `serde`, `rusqlite` and `time` are already
required, and `Cargo.lock` does not change.

## Alternatives considered

- **`OffsetDateTime::now_utc()`, or a `ts` parameter.** Rejected. The first
  panics on a clock this crate could report, and a supervisor that panics loses
  the run it was supervising; the second puts the timestamp of evidence in the
  hands of the party being evidenced.
- **`SELECT COALESCE(MAX(seq), 0) + 1`, or `last_insert_rowid()`.** Rejected: the
  first is two statements with a race between them, and `AUTOINCREMENT` exists
  precisely so a deleted tail never buys its number back — which
  `a_lost_tail_event_never_buys_its_sequence_back` already pins. The second is a
  second round-trip whose value is connection-global rather than row-specific.
- **Inject a clock into `Journal`.** Rejected for now: a fake clock is a knob
  every caller could turn, and the timestamp is testable without one — the test
  bounds the stamp between two clock reads around the call. Reopen if a task
  needs a *fixed* instant (recovery timing is the likely one).
- **`Error::Database`, or a new variant, for a clock with no spelling.**
  Rejected: nothing in SQLite refused anything, and `docs/DESIGN.md` fixes the
  variant list as exact (the same reasoning as ADR-0015).
- **`unwrap` or `unwrap_or_default` on the encoding.** Rejected: a payload that
  failed to encode would either stop the supervisor or be stored as `{}`, and `{}`
  is a lie that decodes as nothing happened.
- **Make the encoder argument public.** Rejected: a public encoder is a way to
  write a row the journal did not derive.
- **Refuse a `task_id` with no row in `tasks`.** Rejected for this task: nothing
  writes `tasks` yet, so the check would reject every event ever appended during
  recovery, before the projection exists. It is a decision for the projection
  task, not an oversight here.

## Consequences

The journal cannot be told what time it is, so no test can assert an exact stamp;
they assert the stamp lies between two readings and ends in `Z`. A later task
that needs a deterministic instant must reopen the clock question rather than
add a parameter to `append`.

`ts` and the envelope's `ts` cannot disagree, because both are one call to
`format(&Rfc3339)` — but `GateAcknowledged`'s own `at` field is the derive's
numeric pair, so the payload column for that entry is unreadable text next to a
readable `ts`. That is `event.rs`'s to fix, recorded here because measuring it is
how the refusal test got written.

The transaction around one `INSERT` is structural rather than observable:
SQLite's own statement atomicity already covers a single statement, and
`RETURNING` does not see an `AFTER INSERT` trigger's write, so no test
distinguishes it. It stays because the task requires it and because the append
that writes event *and* projection row is one statement short of not needing it.

`clock_nanos` is the one line the suite cannot pin, because its value is the
machine's clock rather than a test's: a mutant that reads a clock twice, or reads
it at the wrong moment, survives. What it returns is not out of reach — the sign
of a clock behind the epoch, the nanoseconds either side of a second, and both
refusals are pure functions with tests, and the timestamp test bounds the stamp
between two readings around the call. If a task ever needs an *exact* instant
rather than a bounded one, that is the signal to reopen the clock decision, not
to add a parameter to `append`.

# 0017. Append-only is a trigger on `events`, not a convention

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T017's outcome is that the journal is append-only *in fact*. Until now the rule
was a convention: `Journal` keeps its `Connection` private, and the module
documented that no `UPDATE` and no `DELETE` exists in this codebase. The design
test in VISION.md asks the opposite question — *could an agent ignore this?* —
and a convention in one crate is ignorable by the next crate, by a recovery tool
someone types by hand, and by any program that opens the file. The rule belongs
to the file.

Measured on this toolchain (rusqlite 0.40.2, the bundled SQLite it compiles)
before any of it was asserted:

- A `BEFORE` trigger whose body is `SELECT RAISE(ABORT, 'text')` refuses the
  statement with `SQLITE_CONSTRAINT` (extended `SQLITE_CONSTRAINT_TRIGGER`), and
  the text is the message the caller sees. rusqlite reports the primary code as
  `ErrorCode::ConstraintViolation`.
- `ABORT` undoes the rows the statement had already reached, so a whole-table
  statement changes nothing; the journal stays usable and the next append is
  ordinary.
- `Journal::append` is untouched by the guards: it never names `seq`, and the one
  upsert in the module (`INSERT … ON CONFLICT DO UPDATE`) writes `meta`, not
  `events`. An upsert aimed at `events` *is* refused, because SQLite fires the
  `UPDATE` trigger for `DO UPDATE`.
- `sqlite_sequence` is writable, and SQLite numbers the next row from
  `max(sqlite_sequence, max(rowid)) + 1`. A rolled-back insert does **not** leave
  the counter spent, so a lost tail record cannot be staged that way.
- `PRAGMA recursive_triggers` is off by default, and SQLite fires delete triggers
  for an `INSERT OR REPLACE`'s implicit removal only when it is on. So
  `INSERT OR REPLACE INTO events (seq, …)` with an existing `seq` rewrites a row
  today; measured both ways.

## Decision

Two triggers, created by the same DDL as the table and so present in every file
this build opens:

```text
events_refuse_update  BEFORE UPDATE  ON events → RAISE(ABORT, …never updated)
events_refuse_delete  BEFORE DELETE  ON events → RAISE(ABORT, …never deleted)
```

`BEFORE` so nothing is written before the refusal. `ABORT` rather than `ROLLBACK`
because it undoes only the current statement — a refusal must not take an
unrelated in-flight transaction's work with it — and rather than `FAIL` because
`FAIL` leaves the rows a multi-row statement already reached changed. Only
`events` carries a guard: `tasks` is the queue and `task_state` a projection, and
rewriting those is ordinary work.

The schema version stays `1`. Version 1 means "the schema `docs/DESIGN.md`
gives", and that document now lists the guards; both triggers are
`IF NOT EXISTS`, so a file written before this change gains them on its next open
without a row of its own changing shape. No released journal needs distinguishing
from another.

`docs/DESIGN.md` Database schema gains the two statements, because `journal.rs`
states that its object list *is* that document's list.

## Alternatives considered

- **Leave it a convention, with the private `Connection`.** Rejected: the
  connection is private, the file is not, and the guarantee the journal exists to
  provide cannot depend on which crate holds the handle.
- **Refuse mutations in Rust, above the statement.** Rejected: the same
  convention one layer down, and it cannot see a statement issued elsewhere.
- **`CHECK` constraints.** Rejected: a `CHECK` weighs one row against itself and
  cannot say "this row was already written".
- **A read-only connection, or the rusqlite authorizer.** Rejected: both govern
  one connection, which is the scope that is too small.
- **Guard every table.** Rejected: it would break `stamp_version`'s upsert and the
  projection rebuild the design depends on, and it is not the invariant.
- **Bump the schema version to 2.** Not taken, on the reasoning above. It becomes
  the right call the first time a change is *not* additive — a column that moves,
  or a guard whose absence makes a file unreadable.
- **`PRAGMA recursive_triggers = ON`, or a `BEFORE INSERT` guard for a row that
  names an existing `seq`, to close `INSERT OR REPLACE`.** Not taken: T017 asks
  for the `UPDATE` and `DELETE` refusal, and a pragma changes the pragma set
  `docs/DESIGN.md` fixes. Recorded as a consequence rather than decided here.

## Consequences

Refusal is mechanical and loud: a mutation of a stored event is an error whose
message names the rule, and `events` holds what it held. `Journal::append` is
unaffected, and `docs/DESIGN.md` stays the one statement of the schema.

Two things become harder, deliberately. Nothing can prune or compact the journal
in place: if it ever has to shrink, that is a new decision (a new file plus a
recorded decision), not a `DELETE` someone added to a recovery path. And
`INSERT OR REPLACE` with an explicit `seq` still rewrites a row, because SQLite
fires the delete trigger for its implicit removal only with `recursive_triggers`
on — the fix is that pragma or an insert guard, plus a test. This task did not
choose it, so the next one has to.

`a_lost_tail_event_never_buys_its_sequence_back` (cited by ADR-0016) could no
longer stage its lost record with a `DELETE`, and staging a loss by performing the
mutation the guard exists to forbid would test the wrong thing. It now stages the
loss where a lost record leaves it — the counter ahead of the rows — which
measures the same `AUTOINCREMENT` guarantee. A test that needs a row gone from
`events` cannot get one any more, and should read that as the schema working.

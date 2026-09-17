# 0019. A queue row holds what was asked, and no status

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T019 adds the queue: `Journal::put_tasks(&[Task])` writes a parsed plan into the
`tasks` table, and `Journal::tasks()` reads it back by id. The table itself has
existed since ADR-0015 created the schema idempotently, so the schema is not
what is being decided here. What is being decided is the mapping, and the
mapping is not one-to-one: `docs/DESIGN.md` Database schema gives a row nine
columns, while `Task` has eight fields, and the two sets differ in three places.

- A row has **no status column**, because "A task's status is its `TaskState` in
  `task_state`, derived from the journal like everything else". `Task` has a
  `status` field, so a caller hands one over and something must answer where it
  went.
- A row has **no gate column**. `Task` has a `gate` field, and ADR-0006's rule
  for `title` — a projection of the body, not a stored fact — was never written
  down for a gate, because the field arrived in `task.rs` after the schema was
  fixed.
- A row has a **`protocol` column** that `Task` has no field for.

The task also fixes one rule and leaves its shape open: importing into a
non-empty queue is an error naming the existing task count, with no merge and no
in-place edit. It does not say which `Error` variant carries that, what happens
to the rows an import already wrote before it reached the task that cannot go
in, whether a plan longer than a queue position can be numbered, or what a read
does with a row whose id is not a queue position at all. A file can hold all of
those; `put_tasks` cannot produce any of them.

Measured on this toolchain (rusqlite 0.40.2, the SQLite it bundles) before
choosing, because three of the answers turned out to be facts rather than
preferences:

- `tasks.id` is `INTEGER PRIMARY KEY` **without** `AUTOINCREMENT`, unlike
  `events.seq`. The rowid therefore *is* the id, and no `sqlite_sequence` row
  exists for `tasks` at all. So the journal numbers a queue by counting, not by
  asking SQLite for the next value, and closing and reopening the file cannot
  renumber a task: a queue's ids are stable across a crash by construction.
- A `count(*)` read taken inside a transaction, followed by another connection
  committing a row and then this transaction inserting, fails with
  `SQLITE_BUSY_SNAPSHOT` (measured: `DatabaseBusy`, extended code 517) rather
  than writing. The count and the rows it guards cannot be pulled apart by a
  second writer: the import either owns an empty queue or fails having written
  nothing.
- `added_at` has no SQL default, so a row cannot be written without an instant,
  and ADR-0016 already decided that an instant a caller hands in is an instant a
  caller can move.

## Decision

**A row holds what was asked, and refuses to hold what was concluded.** The
four required sections go into the four columns the design names, the block goes
into `body` byte for byte, and that is the whole of it.

**Status is not stored, and a read reports `TaskStatus::Pending`.**
`put_tasks` ignores the status its argument carries; `tasks()` supplies
`Pending` for every task it returns. This is the ADR-0006 shape applied to the
one field still left with two possible homes: a stored status would be a second
record of a fact whose first record is the journal, and the two could disagree
after any crash between an event and the row. `Pending` is not a guess about the
run — it is the status a task has while nothing has been concluded about it, and
when the `task_state` projection lands it replaces this constant at the one
point where a read materializes a `Task`. Nothing else in the crate will need to
know the projection exists.

**The gate is read back out of the body.** `body` is stored as authored, so the
`**Gate:**` section survives an import and `task::gate_of` recovers the field
from it on the way out, using the same scanner `parse_plan` uses: a label inside
a fenced block marks no gate on either path. `gate_of` is `pub(crate)` rather
than public, because it is the queue's way of not adding a column, not a
question for a caller to ask about an arbitrary string. Adding a `gate` column
would have made a plan carrying `**Gate:**` fail the task's round-trip
done-when, and would have created a copy that a body edit could contradict.

**`title` is written and never read back.** The column exists so a queue listing
does not parse a body to name its rows, so `put_tasks` stores `Task::title()`;
`tasks()` does not select the column, and returns a task whose `title()` is
computed from the body it did read. A stored title that disagreed with its body
would be two facts where ADR-0006 has one.

**`protocol` is written NULL for every row.** `Task` has no protocol field, and
the schema's own comment says NULL "means the configured default", which is the
correct answer for a plan that names none. The column is the place a later
`Task::protocol` will land, and no row written by this build claims a protocol
the plan never asked for.

**A queue's ids are its positions, and a task that arrives under another id is
refused.** `put_tasks` numbers rows from one in document order through
`task::task_id` — the same function `parse_plan` numbers blocks with, made
`pub(crate)` for this reason, so "the third block of the plan" and "task 3 of
the queue" cannot drift apart. A task handed over as `TaskId::new(9)` at
position 2 is refused rather than renumbered: renumbering silently would make
every id a caller quotes a lie about which document it came from.

**The import is one transaction, one instant, all or nothing.** The count, every
`INSERT` and the commit are one transaction, and every row is stamped with one
`stamp_text(clock_nanos())` read inside the call, so a queue's rows are provably
one import rather than three writes that happened to run together. Any refusal —
a non-empty queue, a misnumbered task, a task `validate` refuses — takes back
the rows before it. Half a plan is not a queue: its ids would be the positions
of a document whose earlier half is missing.

**`validate` is asked at the door.** Each task is put through the same predicate
`plan lint` asks before its row is written, because VISION.md §4 promises "a
malformed task never enters the queue" and the queue is the door.

**Every refusal is `Error::Policy` with an empty `paths`,** following
`task::validate`, which already refuses a malformed task exactly this way: a rule
of the queue was broken, nothing was refused *about a file*. The count refusal's
message carries "it holds {existing}", because how many tasks are in the way is
the fact an operator acts on. The count is checked before any task is looked at,
so a refusal against a full queue does not depend on what the second plan
contained.

**The read is `ORDER BY id`, and an id that is no position is damage.** Ordering
is in the SQL, as it is for every read in this file (ADR-0018): queue order is
document order is id order, not the order rows happened to arrive and not the
titles a reader sees. A row numbered below 1 or above `u32::MAX` cannot be a
queue position, and is reported as `Error::Corrupt` quoting the number it
refused — the same judgment ADR-0018 makes about a row that cannot be decoded.
Answering with a truncated id would be the queue quoting a task that does not
exist, which is precisely what a queue exists to make impossible. An empty queue
is an empty `Vec`, not `Error::NotFound`: every project starts with one, and
reading it is the first thing a run does.

The codec lives in a `journal::tasks` submodule rather than in the file's event
half, because it shares a connection and a schema and nothing else, and it is
named for what it holds. The `impl` is on `Journal` all the same, so the queue is
reached through the journal that owns the file rather than through a second door
a caller could hold open beside it.

No dependency is added: `rusqlite` and `time` are already required, and
`Cargo.lock` does not change.

## Alternatives considered

- **A `status` column, updated as work proceeds.** Rejected before being
  weighed: the task states the rule, DESIGN.md states it, and a status marker
  edited into a plan is what VISION.md §4 names as the reason the old `ktask`
  plans went dirty.
- **Storing the status anyway and overwriting it on read.** Rejected: it looks
  identical until a crash, where the stored copy becomes the record of a
  conclusion the journal never recorded.
- **A `gate` column.** Rejected: the body already carries the fact, the round
  trip done-when fails without it, and it is a second copy with no way to be
  reconciled.
- **Reading `title` back.** Rejected: ADR-0006 makes it a projection of the body.
  The column stays written because the cost of computing a title for a listing is
  paid by every reader, and the cost of ignoring a stored one is paid by nobody.
- **`INSERT OR REPLACE`, or merging by title.** Rejected: both are an in-place
  edit of a queue, which the outcome line exists to forbid, and a merge decides
  which of two plans wins a question an operator should answer.
- **Renumbering a task whose id does not match its position.** Rejected: it makes
  the refusal invisible, and an id is how a run is quoted back to an operator.
- **`Error::Conflict`, or a new variant for the count refusal.** Rejected:
  `error.rs` has no such variant, and adding one to say "the queue was not empty"
  would split the refusals of a rule across two variants for no reader's benefit.
  `Policy` already carries the empty `paths` shape `validate` uses.
- **Refusing an empty plan.** Rejected: importing nothing is not a state change,
  and a plan that parses to zero tasks is a document that has no tasks in it —
  not an error, and (asserted) not an import that fills the queue and locks the
  real plan out.
- **`put_tasks` taking `Vec<Task>` by value.** Rejected: the task fixes the
  signature, and borrowing is right anyway — nothing here consumes the caller's
  plan.
- **Ordering by `added_at`, or by insertion order.** Rejected: with the id free
  of `AUTOINCREMENT` those are all facts about *how the rows arrived*, which is
  the fact a queue must not be ordered by. A queue read is ordered by id even for
  rows this build could not have written.
- **A `Queue` type beside `Journal`.** Rejected: it needs the same connection, so
  either `Journal` exposes it or the queue gets a second handle to one file — and
  the guarantees in this file are kept by there being one door.

## Consequences

The queue is SQLite, so nothing about a task is rewritten in place: no statement
in this crate updates a row of `tasks`, and none writes a status. That is now
asserted by tests rather than by intent, and the test that pins it is the one
that refuses a second import and then reads the queue back unchanged.

`tasks()` reports `Pending` for every task, so a caller that shows status today
shows a task as unrun — which is true of every queue this build can produce, but
will stop being true the moment the projection lands. That is the single place to
change, and `the_queue_holds_no_status_so_a_read_reports_the_task_unrun` is the
test that will move with it: it hands over a task marked `Failed` and asserts the
read says `Pending`, so an implementation that starts storing a status has to
change that assertion deliberately rather than by accident.

Three things are deliberately left undone, and recorded as findings rather than
quietly built: `add` of a *single* task into a non-empty queue is refused by this
rule, so the append path (next id, and the `TaskQueued` event that VISION.md
section 3, invariant 3 says is journaled before the side effect) is a later task; no `task_state` row is
written by an import; and `protocol` stays NULL until `Task` grows the field.

Coverage of the codec is by `journal::tasks::tests`, whose `stage_row` helper is
the only way a test reaches a queue state an import cannot produce — rows out of
id order, and an id that is no position. If a later task needs to *replace* a
queue (a re-import after an operator archives the journal), that is a named
operation with its own journal record, not a loosening of this refusal.

# 0024. A rebuild folds the journal before it touches the projection

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T024 adds `Journal::rebuild_state`. ADR-0023 built the projection on the promise
that `task_state` is a summary and not a second source of truth — `docs/DESIGN.md`
Database schema: "a projection and may be dropped and rebuilt by replay", VISION.md
section 5: "materialized current state (a projection, rebuildable from the
journal)". Until this task the promise was load-bearing but unexecuted: nothing
folded `events` back into states. Making it real answers four questions the
documents leave open.

- **What a replay that refuses an event means.** `state::apply` refuses a pair its
  table does not allow, and `append` never consults it (ADR-0016: the journal
  records what it was told). So a journal can be written that no run could have
  walked, and a replay reaches it. The options are to report it, to stop at the
  last state that folded, or to skip the record — and the second is what a fold
  written as a loop does by accident.
- **Where the sequence number goes.** T024 asks that the refusal name the
  offending record. `Error::InvalidTransition` carries the state and the event and
  has no place for a sequence; `Error::Corrupt` carries `seq: Option<u64>` and
  prints it ("corrupt data at seq 12: …"), which is why the journal's four reads
  already use it (ADR-0018).
- **Whether the clear is safe.** A rebuild clears `task_state` and refills it, so
  between the two there is an instant at which tasks have no rows — and an absent
  row is read as "this task has not started", the one answer ADR-0023 says a run
  cannot recover from. ADR-0023 refused that instant for a single row (an upsert
  rather than a delete plus an insert); the same refusal has to be available to a
  whole table or the rebuild is the most dangerous operation in the file.
- **What a `NULL` `task_id` folds onto.** `events.task_id` is nullable by the
  schema, for events about the queue.

Measured on this toolchain (rusqlite 0.40.2 over bundled SQLite 3.53.2) before
choosing:

- `DELETE FROM task_state` inside a transaction that is then refused further down
  leaves every prior row and its `updated_at` untouched (measured: the rows read
  back byte-identical after the rollback), so the instant above can be removed
  entirely rather than only shortened.
- `Connection::transaction()` borrows the connection for the life of the
  transaction, so no `Journal` method — `put_state` included — can be called while
  one is open. `Transaction` derefs to `Connection`, so a free function taking
  `&Connection` can be called by both.

## Decision

**Fold everything before writing anything.** `rebuild_state` replays the whole
journal into a `BTreeMap<TaskId, TaskState>` (one state per task, folded from
`TaskState::Queued` in `seq` order) and only then clears and refills the table. A
journal that cannot be replayed is therefore refused with the projection still
standing, untouched down to its stamps.

**The clear and the refill are one transaction.** The projection a caller can
observe is either the one the run wrote or the fully rebuilt one; there is no
window in which a task has no row. Both writers reach the table through one
private `write_state`, so a replayed row and a row the run wrote are the same
statement's output.

**An event the machine refuses is damage, reported with its sequence.** The
refusal becomes `Error::Corrupt { detail, seq: Some(record) }`, the detail
carrying `apply`'s own words ("illegal transition from `Queued` on event
`TaskDone`"). The journal claims something no run could have done; that is what
`Corrupt` means, and the sequence is the half of the answer an operator can act
on. Folding stops there: keeping the prefix would present a projection of a run
that quietly stopped, which is a statement about a task rather than an error about
a file.

**An event with no task folds onto nothing.** It is in the journal to be read —
by history and log screens — and there is no accumulator it belongs to.

## Alternatives considered

**Fold and write in one pass, calling `put_state` per event.** Shorter by a
function, and it loses on the second bullet above: the clear would have to happen
first (leaving every task rowless until the last write), or not at all (leaving
rows for tasks the journal no longer mentions). ADR-0023 refused exactly this
shape for one row; a rebuild cannot refuse it for the whole table.

**Return `Error::InvalidTransition`.** It is the truer name for what `apply` said,
and it loses the sequence — the one fact that turns "the journal is inconsistent"
into "open the journal at record 41". `Corrupt` keeps both, because the detail
quotes the refusal verbatim.

**Add a variant carrying both.** Honest, and rejected for the reason ADR-0001
gives for fixing `Corrupt::seq` as a field of its own: the location a corrupt
record needs is already part of that variant, and the refusal supplies the words.
A second variant widens the enum every caller above core matches on, to say
something the existing one already says.

**Stop at the refusal and return the states folded so far.** This is the "silently
stopping" the task exists to forbid: a supervisor reading it would see a
projection with tasks missing from the middle and conclude work nobody had done
had not been done.

**Replay one task at a time with `events_for`.** It would need the list of tasks,
which a rebuild cannot have: the projection is exactly what may have been dropped,
and the queue is a separate table whose rows say nothing about which tasks ever
appeared in `events`. The single ordered fold discovers the task set from the
records themselves.

## Consequences

- ADR-0023's "put_state is the only writer of `task_state`" narrows to: no *other*
  writer. `rebuild_state` writes through the same private helper, and a test —
  `a_rebuild_whose_write_is_refused_leaves_the_projection_it_found` — pins that a
  refusal partway leaves the old rows rather than some of the new ones.
- A rebuild costs one read of every event and one write per task, in a single
  transaction: `O(events)` time, `O(tasks)` memory. A run commits a handful of
  events per task, so this is cheap next to the gates, and it is the operation a
  crashed run runs before it does anything else.
- A journal carrying an illegal record cannot be projected at all until a human
  reads the record the refusal named. That is deliberate and it is the sharp edge
  of this decision: repair happens on the journal's contents, not on a projection
  a replay half-believes. Nothing else in the file can fix it, because nothing
  else in the file is allowed to write `events`.
- Rebuilt rows carry the instant the rebuild wrote them, not the instant of the
  event behind them (ADR-0023 chose this so the two are distinguishable). A
  rebuild therefore changes every `updated_at` in the table, and a caller that
  wanted "when did this task really move" must read `events`, which is where that
  question was always answered.

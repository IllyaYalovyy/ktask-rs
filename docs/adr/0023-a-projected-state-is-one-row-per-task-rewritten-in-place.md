# 0023. A projected state is one row per task, rewritten in place

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T023 adds `Journal::put_state`, `Journal::get_state` and `Journal::all_states`.
The table has existed since ADR-0015 created the schema idempotently, so the
schema is not what is being decided here — and neither is the fact that a
projection exists at all: `docs/DESIGN.md` Database schema says a task's status
"is its `TaskState` in `task_state`, derived from the journal like everything
else", and that its rows "may be dropped and rebuilt by replay". What the
document leaves open is everything a write has to answer:

- **What a write does to the row.** `task_state` has no `seq` and no history, so
  either the table holds one row per task that is rewritten in place, or it holds
  every state a task has ever been in and a read picks the newest. The second is
  the shape `events` has, and it is what an append-only instinct suggests.
- **Who dates the row.** `updated_at` is `NOT NULL` with no SQL default, so every
  write must supply an instant, and ADR-0016 already decided that an instant a
  caller hands in is an instant a caller can move. It is not yet decided whether
  that rule reaches a projection, which by definition holds a fact derivative of
  other rows.
- **What an unreadable row means.** `state_json` is `TEXT NOT NULL`, so a file can
  hold JSON that is not a `TaskState` — a hand edit, a build with a variant this
  one does not have, or bytes that moved. So can `task_id`, which is a signed
  `INTEGER` wider than the `u32` a `TaskId` wraps.
- **Whether writing a state is recording.** The recorder calls `put_state` after
  every transition `state::apply` accepts, so the two writes happen side by side,
  and nothing in the design says whether the second one belongs in `events` too.

Measured on this toolchain (rusqlite 0.40.2 over the SQLite 3.53.2 it bundles) before
choosing, because three of these turned out to be facts rather than preferences:

- `task_state.task_id` is `INTEGER PRIMARY KEY` **without** `AUTOINCREMENT`, so it
  is a rowid alias and the file keeps no `sqlite_sequence` row for the table at
  all (measured: `count(*) FROM sqlite_sequence` is `0` after writes). The key is
  therefore a number this build supplies, exactly like `tasks.id` in ADR-0019.
- An `INSERT … ON CONFLICT(task_id) DO UPDATE` is accepted against that rowid
  alias, and reports **one row changed on both arms** — the insert and the
  overwrite are indistinguishable by affected-row count, so the count says nothing
  a caller could act on and is ignored.
- `DELETE FROM task_state` succeeds (measured: `Ok(1)` for the one row held). The
  append-only guards of ADR-0017 sit on `events` alone, which is what lets T024's
  rebuild clear the table — the alternative, "clear it by rebuilding the file",
  would mean dropping a journal that also holds the source of truth.

## Decision

**One row per task, keyed by the task id, rewritten in place.** `put_state` is an
`INSERT` paired with the `ON CONFLICT` update of the row it collided with, so an
overwrite replaces the row rather than leaving a reader to choose between two
answers. One statement is one transaction, so there is no instant — not even a
crash mid-write — at which a task has no state.

**The journal stamps `updated_at`, from the same clock `append` reads.** The same
rule as ADR-0016, for the same reason and with a narrower claim: the column says
when *this file* last learned the fact, and a caller able to move it could move
the projection's own past. It dates a row and orders nothing — `all_states` goes by
task id, because what is current is a set, not a sequence.

**An unreadable row is damage, not absence.** A `state_json` that is not a
`TaskState`, and a `task_id` outside a `TaskId`, are `Error::Corrupt` naming the
task the row claimed, and they stop a whole-projection read. Reading either as
"no state yet" would be the projection claiming a task had not started, which is
the one misreading a run cannot recover from: it would hand the work to a provider
again.

**Writing a state is not recording.** `put_state` inserts no `events` row and
spends no sequence number; the test that pins it asserts that the first event of a
journal is still `seq 1` after four state writes. The event is the fact and the row
is what the facts add up to, and a second event per transition would be two
histories for a replay to agree between.

## Alternatives considered

**Append every state to `task_state` and read the newest.** Loses on the design's
own words: the table is a *projection*, so keeping a history of it duplicates
`events` in a shape with no sequence and no guards, and makes `get_state` an
aggregate. The lifecycle history a screen wants is already in the journal, with a
sequence and a timestamp per record.

**No table: fold the events on every read.** The honest answer for a journal of a
few hundred records, and the wrong one for the read this is. `events_since` exists
(ADR-0018) precisely because a front end polls state on a tick; a poll that
re-folds the whole journal is work that grows with the run, spent to recompute an
answer nothing changed. The fold stays available as `rebuild_state` (T024) — that
is what makes it safe for this table to be a cache.

**Take `updated_at` from the transition's event.** Tempting, and it would make the
two agree by construction. It loses because `put_state` is also how a rebuild
writes state (T024): a replayed row would be dated at the original instant and a
reader could not tell a rebuilt projection from one written as the run happened —
which is exactly the distinction an operator needs after a crash.

**Store status in the `tasks` row.** Rejected already: ADR-0019 refuses a status
column because it makes one fact two homes.

## Consequences

- `Journal::put_state` is the only writer of `task_state` in this crate, and it
  takes `&mut self` as `append` does, so a caller cannot hold the journal open for
  reading while it writes the projection.
- Anyone who writes `task_state` outside this crate — a human in a REPL, a later
  repair tool — is writing a projection: the guards will not stop them, and the
  rebuild is the remedy for anything they get wrong. That is the design's choice,
  not an oversight, and it is why the decode step is a refusal rather than a
  default.
- `Journal::tasks()` still reports `TaskStatus::Pending` for every task, which is
  what ADR-0019 wrote down to be replaced "when the projection lands". It has
  landed, and this task does not replace it: folding twelve `TaskState` variants
  into the five of `TaskStatus` (where is `Running` in that list?) is a decision
  about the `--json` compatibility surface, not a line of a persistence task.
- If a state ever needs to be *reverted* — a decision to un-do a transition — this
  table cannot express it, because it holds one row per task. The journal can, and
  the answer would be to rebuild rather than to add a second shape here.

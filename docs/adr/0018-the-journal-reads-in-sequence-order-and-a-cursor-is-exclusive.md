# 0018. The journal reads in sequence order, and a cursor is exclusive

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T018 adds the read side of the journal: `Journal::events`,
`Journal::events_for(task)` and `Journal::events_since(seq)`, each returning
`Vec<Event>`. The task fixes the signatures and the requirement that all three
order by `seq`; it does not decide four things the signatures cannot express,
and each has a plausible alternative that a later task would have to live with:

- **What "in order" means** when a row carries two candidate orders — the
  sequence the journal handed out and the instant it stamped.
- **Whether a cursor names a record the reader has seen.** T128 polls the journal
  with `events_since(last_seq)` on a tick and its done-when says "the poll never
  re-reads the whole journal", which reads like exclusive but does not say so.
- **Whether a per-task read includes the queue's own records**, whose `task_id`
  is `NULL`.
- **What a read does with a row it cannot decode** — a payload that is not a
  catalog entry, a `ts` that is not RFC 3339, a `task_id` outside a queue
  position, a row whose `kind` column and payload tag name different entries.
  `append` cannot write any of these; a file can hold all of them.

Measured on this toolchain (rusqlite 0.40.2, the SQLite it bundles) before
choosing, because three of the answers turned out to be facts rather than
preferences:

- `seq` is `INTEGER PRIMARY KEY AUTOINCREMENT`, so it **is** the rowid. A scan
  without `ORDER BY` therefore returns rows in sequence order today, which means
  "ordered by sequence" cannot be demonstrated by row order at all — only by
  contradicting it with the instants. The read test stages rows whose `ts` values
  run the other way to their `seq` values, and asserts the pair.
- `ts` carries whatever precision this machine's clock has: three appends back to
  back stamped `.001090933`, `.001312332`, `.001448524`. That is a fact about this
  hardware, not a guarantee, and ADR-0016 already decided a clock can sit on
  either side of the epoch — so two records can be stamped equal, or backwards,
  without anything being wrong with the journal.
- `WHERE task_id = ?1` does not match the queue's own rows: `NULL = 1` is `NULL`,
  and SQLite drops the row. Measured — of three rows (task 1, task 2, queue-level)
  the predicate named two.
- `WHERE seq > 9223372036854775807` matches nothing, and rusqlite has no `ToSql`
  for `u64` at all (ADR-0016). So an `EventSeq` above `i64::MAX` has no binding,
  and the comparison the clamp falls back to answers the same way: empty.

## Decision

**Order by `seq`, in the SQL, in all three reads.** `ORDER BY seq` is part of each
statement, so the order is the database's answer rather than something a caller
has to remember to ask for. `ts` orders a display and never a replay: an instant
is a clock reading, and the journal's own record of what came after what is its
sequence. Ordering by `ts` is not a slower version of the right answer — it is a
different answer, and the one a phase can lose to the attempt that entered it.

**`events_since` is exclusive of its cursor.** `seq` is the newest sequence the
caller already holds, and the answer is strictly `seq > cursor`. Inclusive would
deliver the cursor's own record on every poll, so a TUI polling on a tick would
show every event once per poll. A cursor need not name a record the journal still
holds: a sequence spent by an interrupted commit is still a number a reader is
ahead of, and only the number is asked about. A cursor above the width of the
column is clamped to `i64::MAX`, because no sequence the column can store is ahead
of it and the answer is the same empty read without a conversion failure.

**`events_for` reads one task's records and nothing else.** Queue-level records —
the `task_id` `NULL` `docs/DESIGN.md` documents — appear in `events` and in no
per-task read; the caller that wants both reads both. That is the SQL's own answer
(`NULL` never satisfies `=`), stated here because it is a decision the schema
permits either way and because a caller will otherwise assume a per-task read is a
filter over the whole one.

**A row that cannot be decoded stops the read.** Every refusal is
`Error::Corrupt` carrying the sequence the reader got far enough to know, which is
the variant `error.rs` defines for "durable data was read and could not be
trusted". Four questions, and a row answers all of them or is not read: is the
number a sequence, is the text an RFC 3339 instant, is the number a queue
position, is the payload the entry its own `kind` column names? The last is the
two-columns-for-one-string risk `EventKind::discriminant` exists to keep honest: a
row naming `TaskQueued` in its column and `Resumed` inside its payload cannot be
answered with either half, so it is refused rather than settled. Skipping an
unreadable row is the alternative that keeps a read succeeding, and it is the wrong
one: the caller reading in order to replay would replay less of the run than it was
handed, silently, in the one component whose purpose is to say what happened.

`Error::Corrupt` rather than `Error::Database`, whose documentation mentions "a
journal read returned something the schema cannot describe": nothing in SQLite
refused anything here — the statement succeeded and the *content* contradicts the
schema it was written under. `event_sequence` already reports a sequence that
cannot be a count of events as `Corrupt` for the same reason, and `Corrupt` is the
variant that carries the sequence, which is what an operator needs to go look at.

No dependency is added: `rusqlite`, `serde_json` and `time` are already required,
and `Cargo.lock` does not change.

## Alternatives considered

- **Order by `ts`, or by `ts` then `seq`.** Rejected: an instant is a clock
  reading this build does not control (ADR-0016), and the ordering a replay needs
  is the one the journal issued. Sorting twice to repair the cases where the clock
  and the sequence disagree is a rule about the display, not about the record.
- **`events_since` inclusive of its cursor.** Rejected: a cursor read is "what I
  have not seen", and an inclusive one makes every poll re-deliver one event. The
  caller has to subtract it, which puts the decision in every caller instead of in
  the one place that can state it.
- **`events_since` refusing a cursor it cannot express.** Rejected: the answer to
  "what is ahead of a number larger than every number I can store" is *nothing*,
  and an error would be a caller's cue to treat a healthy journal as damaged —
  the same reasoning ADR-0015 gives for not reporting a *newer* journal as corrupt.
- **`events_for(Option<TaskId>)`,** so a caller could ask for the queue's own
  records. Rejected: the task fixes `TaskId`, and `None` in a read signature means
  "no filter" to a reader, which is `events` under a different name.
- **Filtering in Rust over `events()`.** Rejected: it reads the whole journal to
  answer a question about one task or one cursor, and T128's done-when forbids a
  poll re-reading the journal. It would also decode rows it then throws away, so a
  corrupt row in an unrelated task would stop a read that never wanted it.
- **`impl Iterator` instead of `Vec`.** Rejected: the task fixes `Vec<Event>`, and
  a streaming read needs a live statement, which needs a lifetime on the return
  type that every caller above this one would have to carry. A journal read that
  wants a window asks for a cursor; the rows that fit a screen are already small.
- **Skip unreadable rows, or return them as a placeholder entry.** Rejected: both
  make a partial journal indistinguishable from a complete one, and a placeholder
  that decodes is a fabricated event in the record of a run.
- **`prepare_cached` instead of `prepare`.** Not taken now: the three statements are
  one per read call, and no task polls them yet. It is the shape of the statement
  text (a constant per read) that makes it safe to add later without changing what
  any read answers.

## Consequences

`events()` returns the whole journal, so a caller that reads on a tick wants
`events_since` — the read side is cheap to call and bounded in what it reads, and
nothing stops a caller from asking for everything. T128 can rely on the cursor
being exclusive; if it ever wants the record at its own cursor it reads `events()`
once at start-up.

A corrupt row now stops three reads rather than being visible in one and absent
from another, so the TUI's first render after a hand-edited journal is an error
naming a sequence. That is the intended failure and it is testable — five tests
cover the four questions and the one row whose halves disagree.

`decode_event` is a private pure function over a private `EventRow`, so every
refusal is asserted without opening a file, and the row-staging helper in the
tests is the only way a test reaches a state `append` cannot produce. If a later
task needs a *partial* read (a journal readable up to the first damaged record),
that is a second function with its own name and a caller that decides what to do
with the gap — not a loosening of these three.

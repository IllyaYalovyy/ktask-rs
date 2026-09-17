# 0020. A streaming read hands a record over and keeps none

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T020 adds `Journal::for_each_event(from, &mut dyn FnMut(Event) -> Result<()>)`,
whose stated outcome is that "reading a large journal does not load it all into
memory". Three reads already return `Vec<Event>` (ADR-0018), and the readers that
are coming want the journal itself, not a screenful of it: the history and logs
screens of VISION.md §13, and the §12 poll that lets a TUI see a run started in
another process. A journal grows for as long as a project lives and nothing ever
deletes from it (ADR-0017), so "read the journal" is an unbounded request, and a
reader that pays for the whole file to answer "what happened next" pays it on
every tick.

ADR-0018 rejected `impl Iterator` over `Vec` because a streaming read needs a live
statement, and a live statement is a lifetime that every caller above this one
would carry. A callback is the same streaming read with no lifetime to carry: the
statement is alive only inside the call, so the caller cannot hold one, and cannot
name one.

Measured on this toolchain (rusqlite 0.40.2, its bundled SQLite, debug profile)
before choosing, because the size the read is charged for is a fact:

- `size_of::<Event>()` is 88 bytes, so a ten-thousand-record `Vec` is 860 KiB of
  envelope before the `title` and `commit` strings each record owns. The journal
  those ten thousand records live in is 1 064 KiB: the read costs about the file.
- Reading them through `for_each_event` grew this process's resident set by
  32 KiB at its peak and no more, whatever the journal's length, because one
  record is alive at a time. Collecting the same ten thousand afterwards cost
  267 KiB — less than the 860 KiB above, because this process had just written
  those records and the allocator reused what it had already mapped. The deltas
  understate a collected read; the shape does not.
- A callback that refuses a record after 25 of 10 000 stops the read after 25. A
  read that gathers before it hands anything cannot stop at 25, and a caller that
  wants to stop early (a screen full, a matching event, a shutdown) is the reason
  the read is handed records rather than a collection.

## Decision

**One prepared statement, stepped.** `for_each_event` prepares
`SELECT … WHERE seq > ?1 ORDER BY seq` and iterates `Rows::next`, decoding one
row, handing the `Event` to the callback, and dropping it before stepping again.
Nothing collects. The collected reads keep their signatures; `read_events` is now
built on the same runner with a callback that pushes onto a `Vec`, so a `Vec` is
what a caller asked for rather than what a read assumed, and there is still one
row shape and one decode rule for all four reads.

**The cursor means exactly what `events_since` means.** Exclusive of `from`,
ordered by `seq`, clamped to `i64::MAX` past the width of the column, and a cursor
that names a lost or never-existing record is answerable (ADR-0018). Two cursor
reads that disagreed about their cursor would be one bug waiting in the poll that
uses both: the caller's cursor is one number and it is fed to whichever read it
needs.

**The callback's error is the read's error, and the read stops.** `f`'s `Err` is
returned unchanged — not wrapped, not downgraded to a `Database` failure — after
the records already handed over stay handed over. A reader that took ten thousand
records and refused the next has read ten thousand; the journal holds no row the
read wrote and has spent no sequence. There is no partial-read variant that
reports "some records and a damage report": a row that cannot be decoded is
`Error::Corrupt` naming its sequence, as in every other read, because the caller
reading in order to replay must not be handed a journal with a record quietly
skipped.

No dependency is added: `rusqlite`, `serde_json` and `time` are already required,
and `Cargo.lock` does not change.

## Alternatives considered

- **`impl Iterator<Item = Result<Event>> + '_`.** Rejected on the same ground
  ADR-0018 gives for `impl Iterator`: the iterator borrows the statement, so it
  borrows the journal, and every caller — the TUI's follow mode in particular —
  would carry that borrow. It is worse than a lifetime, though: SQLite holds the
  read snapshot open while a read statement is unfinished, so an iterator a caller
  parks between polls holds that snapshot open for as long as it is parked, and a
  parked iterator is easy to write by accident. A callback cannot outlive its call,
  so it cannot park one.
- **`Vec` with a `LIMIT`, paging by cursor.** Rejected: the caller ends up
  re-implementing this read, badly — a page size is a guess at the reader's memory,
  and the loop that asks for the next page is the same row iteration this task is
  asked to write once.
- **`for_each_event` taking `impl FnMut(Event) -> Result<()>` instead of
  `&mut dyn FnMut`.** The task fixes the signature, and it is the better choice
  anyway: the state a reader keeps (a cursor, a count, a screen) outlives the call
  while the closure need not, and a `dyn` parameter keeps the read one monomorphized
  function rather than one per callback in the binary.
- **Collecting anyway, then calling `f` over the `Vec`.** Rejected: it satisfies
  every signature in this task and defeats its outcome, which is the whole point of
  the task. It is also not merely slower — it makes "read the journal" a request
  whose cost is the journal, which is what §12's poll must not do.
- **`events()` rewritten to return `impl Iterator`.** Rejected: `events()` is a
  compatibility surface for scripts and a caller that wants a stream calls
  `for_each_event`. A caller that wants to sort, index or `len()` a journal reads a
  `Vec`.
- **A read-only connection, or a second connection for readers.** Not taken: a
  second connection is a decision about WAL checkpointing and writer contention
  that no caller has forced yet, and the append-only guarantees ADR-0017 puts in the
  file hold whoever opens it. Recorded here so that whoever reaches for it next
  knows it was looked at and left.

## Consequences

The four reads now decode through `for_each_read`, so a change to the row shape
reaches all of them at once — which is the point, and also means a mistake in that
one function is a mistake in every read. What covers it is the nine tests in
`journal::tests::streaming` and the read tests in `journal::tests` that went through
`read_events` before it did.

`ORDER BY seq` in the cursor statement cannot be tested by removing it: `seq` is
`INTEGER PRIMARY KEY AUTOINCREMENT`, so it *is* the rowid (ADR-0018 measured that
a scan already returns rowid order), and a cursor read scans the primary key. The
mutant survives because it is equivalent, not because the suite is thin; it is
killed by contradicting the *instants* instead, which is how ADR-0018 handles the
same fact. The ordering assertion for the stream is
`the_stream_arrives_in_the_order_the_journal_numbered_its_records`.

A caller that streams cannot hand the journal to someone else to hold: the records
are gone the moment the callback returns one. Anything that needs to keep them (a
sort, a diff of two reads, a snapshot to render twice) collects them itself and pays
for what it keeps, deliberately. T128's poll wants the cursor read as a stream; the
history screen wants it as a window, and a window over a stream is a callback with a
counter in it.

`for_each_event` is the only read whose cost does not grow with the journal, so
where a later task is choosing how to walk the file, that is the one to reach for.
If a future reader needs to stream a *per-task* window, the honest addition is
another public read over the same runner with `WHERE task_id = ?1 AND seq > ?2` —
not a boolean flag on this one, which would make the cursor mean two things.

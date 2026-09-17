# 0006. A title is a character-count projection of the body

- **Status:** accepted
- **Date:** 2026-09-17

## Context

Every interface that lists the queue needs a short label for a task: the task
list, `ktask-rs status`, the `TaskQueued` journal payload. The label has to come
from somewhere, and a task's `body` is the block as authored, whose first line
is its heading — so the label is available without asking anyone to write a
second, disagreeing summary.

`docs/DESIGN.md` Database schema stores `title` as a column. The in-memory model
carries no title field, only `body`. That is not an accident to be fixed later:
an import writes the column from the same line, and a stored copy of a derived
value is a value that can drift from what it was derived from.

Three things then have to be settled once, because the answer shows up in the
queue list of every run from here on:

- **What a "character" is.** Bytes split a multi-byte character in half, and a
  Rust `&str` borrowed from `body` cannot be returned pointing into the middle
  of one — the cut either lands on a boundary or the borrow is impossible.
- **Whether the cut is marked.** An ellipsis is the convention, and it is
  also three characters (or one, at the cost of a different character set in
  the output).
- **Whose job width is.** A terminal cell is not a character: an ideograph is
  two cells wide, and the number of cells available belongs to the pane doing
  the drawing, not to the queue entry.

## Decision

**`Task::title()` returns the first line of `body`, cut to the first 80
characters, where a character is a Unicode scalar value.** It borrows from
`body` and allocates nothing. It appends nothing.

- The unit is characters, not bytes: 80 Latin letters and 80 ideographs each
  fill the same slot, and the cut is always on a character boundary.
- 80 is the narrowest terminal `docs/CONTRACT.md` promises to render fully, so
  a title cut here fits every supported screen without anyone asking.
- The TUI still truncates again to fit its own pane, using display width
  (`text.rs`, unicode-safe). That is not this function's concern: it cannot know
  the pane, and a core type that guessed the pane's width would be wrong half
  the time.

## Alternatives considered

- **Cut at 80 bytes.** One comparison instead of a scan, and it produces an
  invalid string for any task written outside ASCII. Rejected outright: the
  function returns `&str`, so the only way to cut mid-character is to panic on
  the way out.
- **Cut at 80 grapheme clusters, or 80 cells.** Correct for a wider range of
  text, and wrong for the reason that matters: a cell count is a property of
  the screen. `text.rs` already does display-width fitting where the screen is
  known. Two rules in one function would mean the core silently duplicated the
  TUI's.
- **Append `…` when cut.** Costs a character from a budget measured in
  characters, and makes the returned text not a prefix of `body` — so the title
  could no longer be checked against the body it came from. Frontends that want
  an ellipsis can add one after their own fit.
- **Store a `title` field on `Task`.** Rejected for the drift already stated:
  the column and the projection are written from the same line by the import,
  and only one of them is the authority.

## Consequences

- A title is always a prefix of the first line of `body`, so a test can compare
  it against the body rather than against a second hand-written string.
- Titles are stable across refactors of the surrounding text; there is no
  summary to keep in sync with the task it summarises.
- `Task` derives nothing but `Debug`, `Clone`, `PartialEq`, `Eq`: the queue
  stores the fields as columns and the journal stores state, so nothing asks
  for a serialized `Task` yet. When something does, that is a data-format
  decision and takes its own ADR.
- If a task heading is ever allowed to be a summary distinct from its first
  line, this function stops being the whole answer and the stored column takes
  over; the import task is where that would surface.

# 0008. The required-section rule is one predicate, reported two ways

- **Status:** accepted
- **Date:** 2026-09-17

## Context

ADR-0007 left a note for a later task: `plan lint` needs a per-task check over
tasks already in the database, where no parse happens, and "the section-presence
rule in this file is what it should call rather than restate". T009 is that
check: `validate(task) -> Result<()>`.

Two things made it a decision rather than a function.

The first is where the rule lives. The rule already existed, inside
`build_task`, reachable only from `parse_plan`. Copying it into a new function
would give one fact two definitions on two different representations — a label
in a `BTreeMap` versus a field of `Task` — and the two drift the way such pairs
always drift. The existing pair of reports also disagreed about what counts as
present: the parser asked whether a label had been seen, and a queue row can
only be asked whether its field holds text, so `**Refs:**` written with nothing
under it imported as a task with no refs.

The second is which error answers it. `docs/DESIGN.md` fixes the `Error` enum
(ADR-0001) and T009 names `Error::Policy` for the validation. `parse_plan`
already returns `Error::NotFound`, on the reasoning recorded in ADR-0007, and
its message carries the line number a reader opens — information that exists
only while a document is being read and is worth nothing once the row is in
the database.

## Decision

**One predicate, two reports.**

- `missing_sections(task)` is the whole rule: the four fields the format
  requires, in the order the format names them, and any of them blank is
  missing. It is asked of a `Task`, not of a block, because the task is what
  both the queue and the parser end up holding.
- `validate(task)` is that predicate for everyone who already has a task —
  `add` before it writes a row, `plan lint` over every row. It reports
  `Error::Policy`, because the rule a queue row broke is the queue's own, and
  it names **every** missing section, not the first: a check that reports one
  mistake per run is a check nobody trusts to have looked.
- `build_task` asks the same predicate and reports `Error::NotFound` with the
  heading and line, unchanged from ADR-0007. Same fact, different reader: one
  reader is holding a document and needs a line, the other is holding a queue
  and needs a task id.
- **A required section that is blank is a missing section.** A `**Verify:**`
  with nothing under it names no command, and the queue keeps a task because
  something mechanical can be proved about it. This tightens the import: a
  block that previously slipped through with an empty `Verify:` is now refused
  at the line that wrote it.
- **`**Gate:**` keeps the opposite rule** — present-with-no-text is still a
  gate (ADR-0007). The asymmetry is deliberate: `Gate:` marks the task one a
  person must decide, so its presence is the fact and its text is a courtesy,
  while the other four are evidence.
- `Task`'s four fields stay `String`, not a non-empty type. A row read out of
  the database is whatever the database held; the check is a function a caller
  can run and report, not a type that makes a read fail.

## Alternatives considered

- **Restate the rule inside `validate`.** Shortest diff. Rejected for the reason
  ADR-0007 already wrote down: two definitions of one rule, and only one of
  them is ever tested when the other changes.
- **Have `parse_plan` call `validate` and forward its error.** One call site
  instead of two, but it throws away the line number to gain nothing — the
  predicate is shared either way — and it rewrites the message ADR-0007
  settled and its tests pin. The import keeps its own report.
- **`Error::NotFound` for `validate` too.** Consistent with the import, and it
  renders tidily. Rejected: `NotFound` is what the parser says about a document
  ("the `Refs:` section of `## T013`"), while a row of the queue that cannot be
  queued has broken the queue's rule, which is what `Policy` is for. T009
  names `Policy` for the same reason.
- **Treat blank as present in `validate`,** matching the old import behaviour.
  Rejected: it is how a task with no `Verify:` gets into the queue, which is the
  one thing VISION.md §4 forbids.
- **Four `Option<String>` fields, or a `NonEmptyString` type.** It moves the
  check into the type system, at the cost of a read that fails instead of a row
  that is reported. `plan lint` prints one line per problem; it cannot print a
  line for a row it could not load.

## Consequences

- `plan lint` (T113) calls `ktask_core::validate` and adds the
  checks that belong to it and not here: a `Verify:` that parses, a `Refs:`
  path that exists, no duplicate ids.
- Someone who wrote `**Verify:**` as a heading with the command below it now
  learns at import rather than never. Nothing in this repository's own task
  list is written that way.
- `Error::Policy` renders its path list even when the list is empty, so the
  message ends `(offending paths: )`. No path broke this rule — a queue row did
  — and the tests pin the current rendering, so whoever fixes the empty case in
  `error.rs` will find them. Fixing it belongs to `error.rs`, not to a task
  about the task format.
- The `missing_sections` predicate reads fields, so a task assembled by hand
  with the four fields filled validates even though no document was read. That
  is the point: the queue, not the plan file, is what is being checked.

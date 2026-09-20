# 0063. One journaled record per attempt, ordered by attempt and attributed to its task

- **Status:** accepted
- **Date:** 2026-09-20

## Context

VISION.md §6 makes an attempt's record a thing in its own right — "every
attempt is preserved separately: executor session ID, timestamps, configured
and provider-reported model IDs, exit reason, commands run, gate results, git
SHAs, tokens, and cost" — and §7 makes it load-bearing: a failure bundle is
assembled from *the prior attempt's* evidence, which is only possible if a
retry left the first attempt's evidence alone. T068 fixes the type and the
catalog entry `docs/DESIGN.md` already names for it (`AttemptRecorded`, whose
one payload field is `record: AttemptRecord`) and leaves six decisions that
neither the task body nor the design document makes.

- **Where the evidence may live.** `docs/DESIGN.md` gives the database four
  tables and `events` is the only append-only one. ADR-0024 makes the other
  three projections: `rebuild_state` drops what they hold and recomputes it
  from the events, and ADR-0017's two triggers are what make an append-only
  claim stronger than a convention.
- **What the entry does to the machine.** ADR-0022 gives every state one
  exhaustive match, and ADR-0011 admits a catalog entry only alongside the
  `apply` arm that answers it. An attempt's record is not a transition — it
  says what an attempt was, not what happens next — so what a state is
  supposed to answer it with is undefined, and the 20-state × 20-entry sweep
  in `state.rs` forces an answer for every pair.
- **What order history has.** ADR-0016: `Journal::append` never consults the
  machine, so a record is written when its recorder reaches it. A journal can
  therefore hold attempt 3's evidence in an earlier row than attempt 2's.
- **Which task a record belongs to.** Two suppliers name it: the row's
  `task_id` column, which the caller of `append` sets, and
  `AttemptRecord::task`, which the attempt itself reports. Nothing on the way
  in compares them.
- **What a float does to the event type.** The record carries `Usage`, whose
  `cost_usd` is an `Option<f64>` because that is the form a provider's
  telemetry answers in (ADR-0049 refused to stand a zero in for an unreported
  figure). `EventKind` has been `Eq` since T012, and a float is not an
  equivalence relation.
- **How big a variant may be.** Measured on this toolchain (clippy 1.98.0,
  `clippy.toml` unchanged) with a throwaway probe, because neither number was
  knowable without compiling: `size_of::<AttemptRecord>()` is 280 bytes,
  `size_of::<EventKind>()` is 56. An unboxed `Record(AttemptRecord)` variant
  among otherwise small ones is a hard error — `clippy::large_enum_variant`,
  "the largest variant contains at least 280 bytes", `-D
  clippy::large-enum-variant` implied by `-D clippy::all`, which
  `Cargo.toml` denies — and clippy's own suggestion is `Box`.

## Decision

**One attempt's whole evidence is one event.** `AttemptRecorded` carries a
complete `AttemptRecord`, and the journal is the only place it is stored: it is
evidence a rebuild must be able to reproduce, and only `events` survives one.
One event per attempt, not one per field — the record is what an attempt
proved, and a reader should not have to remember which of four entries
happened to be journaled while it ran.

**`apply` answers it with the state it was asked from, aimed at the attempt the
record names.** `record.id == attempt`, refused otherwise — the same shape
`Running` and `Remediating` already use for `AgentOutput` and `VerifyFailed`
(ADR-0022), which is the precedent for "evidence about this attempt that
changes nothing". So filing a record moves the run nowhere, and replaying a
journal that already holds one changes nothing. `Queued`, `Preflight`,
`PublishedVerified`, `Paused` and the four terminal states have no attempt held
and refuse it outright; three `LEGAL` rows are added (49 → 52) and the sweep now
answers 240 pairs.

**A record is therefore filed while its attempt is still held.** That is a real
constraint on the runner: the record for an attempt that failed goes in before
`TaskFailed`, and the record for one that succeeded before `TaskDone` or
`PublishVerified` hands off to `PublishedVerified`. It is the right constraint
rather than an inconvenience — a state that says the run is over refusing
evidence about a run it says is over is the machine noticing an ordering
mistake, which is what these arms exist to notice.

**`attempt_records(journal, task)` orders by `AttemptRecord::id`, not by
sequence.** The order a reader comparing one attempt with the next wants is the
order the attempts ran, and journal order is not that (ADR-0016). The sort is
stable, so two rows that attempt ordering does not explain stay as written
rather than being reordered by something no one chose.

**A row whose record names a different task is refused, never skipped.**
`Error::Corrupt` naming the sequence, the attempt and both task ids. Skipping it
would hand a reader a history and let it guess which half of a row to
disbelieve, and a task growing an attempt it never ran is precisely the
fabrication this read exists to prevent. Repair starts from knowing which of the
two claims is the wrong one, so the message says both.

**`EventKind` and `Event` are `PartialEq`, not `Eq`.** Cost stays a float, so
the journal holds the number the provider reported rather than a number this
tool rounded into microdollars and then believed. The removal is contained:
`Usage` has been `PartialEq` since ADR-0049 defined it, nothing outside
`ktask-core` required `Eq` on an event (measured — the workspace builds and
clippy passes with the derive gone), and equality between two events is a
"same observation" question, which floats answer well enough for testing and
not at all for logic. Nothing in this crate branches on event equality.

**The payload is `Box<AttemptRecord>`.** Boxing is clippy's own suggested fix,
costs one allocation per attempt, and leaves the bytes alone: `serde` writes a
`Box` exactly as it writes what it holds, so a journal row is still the
`record: AttemptRecord` `docs/DESIGN.md` spells, and `event.rs`'s encoding test
asserts the field names and the round trip. `TaskState::Paused` boxes its
`resume_to` for the same reason.

**The record refuses a key the type does not have**, the way a configuration
document does (`#[serde(deny_unknown_fields)]`). Durable data that decodes
leniently is durable data that loses a field silently the first time writer and
reader disagree, and a journal that cannot be read is worse than one that
cannot be written.

**`exit_reason` stays a `String`.** `FailureClass` is the classified half of a
failure and is journaled by the transition that fails the task; this is the
sentence beside it, in the words of whoever watched the process stop, which is
also why a green attempt has one ("exited 0") rather than nothing. A second
enum here would be a second vocabulary for one fact.

## Alternatives considered

- **An `attempts` side table.** It reads as the natural home for a record, and
  it cannot work: ADR-0024 rebuilds the projections from the events alone, so a
  table of its own would hold the only copy of evidence a repair cannot
  reproduce — the one failure this project exists to prevent. `docs/DESIGN.md`
  fixes four tables anyway.
- **Update one row per task on each retry.** Exactly what VISION.md §7 forbids:
  a failure bundle assembled after a retry needs the first attempt's session
  id, its model ids and its gate results, and there would be nothing left to
  assemble them from. ADR-0017's triggers refuse the update regardless.
- **Spread the evidence over entries already journaled** (`AttemptStarted`,
  `GateFinished`, `AttemptFinished`). Each half exists somewhere, and no
  single row then says what one attempt proved; a reader would reconstruct
  which gate result belonged to which attempt from adjacency, across an
  attempt boundary, in every consumer. `AttemptFinished` carries `usage` and
  nothing else.
- **Let every state answer a record with itself.** It is the least code and it
  makes the sweep quiet, and it means a journal could claim a task that never
  ran held evidence about an attempt it never had. `Paused` refusing is the
  case that decides it: a pause is a wait, not a run, so there is nothing in
  flight for the evidence to be about.
- **Order by `seq`, or return unordered rows and let callers sort.** Journal
  order is an artefact of when a recorder got there, and "let callers sort" is
  a rule every consumer has to remember and one of them will forget; the task
  history is one function with one order.
- **Drop the misattributed row, or trust the record's own `task` and re-key
  the row.** Dropping hides the corruption; re-keying lets a caller's mistake
  about the row move evidence between tasks, which is worse than refusing.
- **Store cost in integer microdollars to keep `Eq`.** A cheaper type for the
  derive, and the journal would stop holding the number the provider said —
  ADR-0049 already decided an unreported or reported figure is recorded as
  reported. `UsageSource` keeps the two ways to have nothing apart, and that
  distinction does not depend on the derive.
- **`#[allow(clippy::large_enum_variant)]` on the variant.** Forbidden by the
  project's own rule against silencing a lint at the line that triggered it,
  and unnecessary: the box is the fix clippy proposes, is already the pattern
  at `TaskState::Paused`, and is invisible in the serialized bytes.

## Consequences

- `attempt.rs` owns an attempt's evidence and the one read over it. The TUI
  failures screen, `--json` and the failure bundle all get their attempt
  history from `attempt_records` rather than each folding the journal
  themselves — and none of them call it yet, so the wiring is later tasks'.
- The runner owes an ordering: file the record while the attempt is held. The
  refusals are the enforcement, and they are cheap to hit by accident, so the
  rule is stated in the `Paused` and `PublishedVerified` doc comments where a
  caller reading the machine will meet them.
- `AttemptRecord` is durable data. Adding a field is a change to what a journal
  written today means, and `deny_unknown_fields` guarantees an old reader
  refuses a newer row rather than misreading it — the loud direction, on
  purpose.
- Event equality is now partial. Anything wanting a total order over events
  should key on `Event::seq`, which is what the journal already treats as an
  identity; if a real need for `Eq` appears it needs a cost type that is one,
  not a `#[derive]` on a float.
- **A gap this task reported rather than filled.** VISION.md §6 also lists
  "commands run" among an attempt's preserved evidence, and neither the record
  shape T068 fixes nor `GateResult` carries one: a gate result names the
  `GateKind` that ran and reports what the command wrote, while the words live
  in `Gate::command`, which is configuration read before the run. Putting a
  command line in the journal would also put a secret in it — `docs/DESIGN.md`
  redacts journal text and VISION.md §16 counts an operational command line
  among what must not leak into the repository. Resolving it needs a decision
  about redaction, not a field, and belongs to whoever owns §6's remaining
  evidence.

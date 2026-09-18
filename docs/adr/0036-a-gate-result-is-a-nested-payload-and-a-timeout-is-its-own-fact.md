# 0036. A gate result is a nested payload, and a timeout is its own fact

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T040 defines `GateResult` in `crates/ktask-core/src/gate.rs`: the eight fields a
gate run leaves behind. The task fixes the field list and gives two completion
checks — "the type serializes into an event payload" and "a timed-out result is
distinguishable from a non-zero exit" — and says nothing about the bytes. Three
things already in the repository decide those bytes, and one of them looks like
it decides more than it does.

1. `docs/DESIGN.md` gives the `GateFinished` catalog entry the payload
   `result: GateResult`, and states that no payload may be added "without
   extending `state::apply` in the same task" (`docs/DESIGN.md:214`).
   `event.rs` asserts that entry's absence: its `DEFERRED_PAYLOADS` table
   decodes `{"kind":"GateFinished",…}` on purpose and requires a refusal.
2. ADR-0011 records why that deferral is load-bearing rather than clerical: an
   entry with no producer and no `apply` arm is an event that *can* be journaled
   whose effect on state nobody specified, which is the one artifact replay
   cannot reproduce. ADR-0022 makes the cost concrete — `apply` is one match per
   non-terminal state with no wildcard arm, eight helpers (`from_queued` through
   `from_paused`), plus the terminal refusals and `EventKind::discriminant`.
   Adding a variant means answering it in every one of those places.
3. What a finished gate does to a task's state is not written anywhere. The
   catalog carries a separate `VerifyFailed { class, detail }` for the failure a
   refused gate causes, and `GateAcknowledged` — the one gate event that exists —
   moves a task only out of a `HumanGate` pause. So the answers a
   `GateFinished` arm needs would be invented by the task that only has to
   define a struct, in the file list of which `state.rs` does not appear.

The timeout question is independent and cannot be left to the producer, because
the *shape* of the answer is what makes it available at all. A gate that runs
out of its budget is killed by the runner, so its wait status reads "killed by a
signal" — byte for byte what an out-of-memory kill reads. A non-zero exit reads
differently again: the command ran, answered, and refused. `Gate::timeout_secs`
already promises that such a run is "reported as timed out rather than as
failed".

Measured while writing this, rather than reasoned: serde's `Option<T>` treats a
key that is absent as the same fact as a key holding `null`, so a payload
missing `exit_code` decodes as `None`. That is not a hole to plug — it is the
convention `event.rs` already pins for the envelope's absent `task_id` ("an
absent `task_id` and a null one are the same fact").

## Decision

`GateResult` is defined in `gate.rs` with the eight fields named by the task,
`Serialize`/`Deserialize` over them, and `deny_unknown_fields`. It is nested
data, and `EventKind` gains no variant here.

- **Nested, not flat.** A result is one JSON object under the one key
  `docs/DESIGN.md` names: `{"kind":"GateFinished","result":{"kind":"Verify",…}}`.
  Nesting also keeps the gate's own `kind` out of the object the payload tag
  already owns — a payload field named `kind` beside `#[serde(tag = "kind")]` is
  the collision ADR-0011 measured and the derive refused. A test in `gate.rs`
  builds exactly that document and reads the nested object back, so the entry,
  whenever it lands, has to embed these bytes unchanged or fail a test.
- **A timeout is its own field.** `timed_out` carries the fact; `exit_code`
  stays `None` because a killed command produced no status, and `signal` records
  the kill. A timeout, an outside kill and a non-zero exit are three payloads.
- **The write side always spells both halves.** No `skip_serializing_if` on
  `exit_code`/`signal`, unlike `Gate::working_dir` and `Gate::env`: those live
  in a document an operator edits, this one lives only in the journal. The read
  side accepts a missing key as nothing, which is the envelope's rule and is
  asserted as such.
- **`passed` is stored, not derived.** It is the runner's verdict, and not
  always `exit_code == 0`: a `Flake` gate decides over `flake_runs` runs and a
  `Privacy` gate reads its own report.

## Alternatives considered

- **Add `GateFinished` now, since `GateResult` exists at last.** Rejected: the
  answers its `apply` arms need — one in each of the eight per-state helpers,
  plus the terminal refusals — are a state-machine decision no document has
  made, and the rule at `docs/DESIGN.md:214` exists precisely to stop an entry
  arriving ahead of its producer. ADR-0011's consequences line
  expected T038 to land it; T038's file list was `gate.rs` and it wrote the
  configuration half. The debt moves to the task that emits the event, and is
  reported as a finding by this one.
- **Encode the timeout as the shell's 124.** Rejected: the runner kills the
  process group itself rather than asking a shell to time it, so 124 is a number
  this code invented that no wait status will ever carry — while a gate command
  that exits 124 of its own accord would become indistinguishable from a
  timeout, which is the exact confusion the completion check forbids.
- **Drop `timed_out` and let `exit_code: None` mean it.** Rejected: that is also
  the spelling of a signal kill, so a budget that ran out and an OOM kill would
  be journaled as the same event.
- **Tolerate unknown fields, for forward compatibility.** Rejected: `Config`,
  `Gate` and `Profile` all refuse an unknown key, VISION.md §2 takes backward
  compatibility out of scope, and a silently dropped field inside the record of
  what a gate said is a concealed failure — invariant 5.
- **Refuse a payload that omits `exit_code`.** Rejected after measuring serde's
  behaviour rather than assuming it: absent and null are one fact here, and
  pretending otherwise would have meant a hand-written `Deserialize` for a
  distinction `event.rs` has already decided not to draw.
- **Flatten the result into the payload** (`#[serde(flatten)]`, so a gate's
  fields sit beside the tag). Rejected: the document names one `result` key, and
  flattening moves the gate's own `kind` into the object the tag occupies — the
  collision above, for no byte this journal needed.

## Consequences

The wire shape of a gate result is fixed by tests before anything can emit one,
so a later rename of a field is a failing test rather than a journal that quietly
reads differently. `gate.rs` gained seven tests, all of them about bytes.

Both gate catalog entries are now blocked on a producer rather than on a payload
type: `GateStarted` and `GateFinished` need the task that runs a gate, and
`event.rs`'s `DEFERRED_PAYLOADS` table is where the move is proved when that task
lands them with their `apply` arms.

`passed` being stored means the rule "a passed gate reported 0 and did not time
out" belongs to `run_gate()`, which is where the facts are; a constructor there
is the cheapest place to keep it. Nothing in this type refuses
`passed: true` beside `exit_code: Some(1)`, and no test pretends it does.

`stdout` and `stderr` are retained whole. Redaction is not this type's job:
ADR-0032 puts redaction inside the journal's write path, so a gate result reaches
its row with secrets already masked whatever the producer put in. Truncating
output is a display decision for the TUI, and a limit there must not become a
limit here.

If a field is ever added, the decisions above still bind it: write it `null` when
absent, refuse it when unknown, and keep it inside the nested object.

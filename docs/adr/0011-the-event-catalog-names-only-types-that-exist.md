# 0011. The event catalog names only types that exist

- **Status:** accepted
- **Date:** 2026-09-17

## Context

`docs/DESIGN.md` fixes the event catalog as 28 entries, and T012 is told to
define "every variant whose payload fields are types that already exist" while
leaving out a list of eight: `GateFinished`, `AttemptFinished`,
`AttemptRecorded`, `ProviderDetected`, `TddExceptionUsed`, `DecisionRaised`,
`DecisionResolved` and `SelfHealingReport`. The two rules do not pick the same
set, and the mismatch is not cosmetic — it decides what the first journal on
disk can contain.

Checking each payload against the crate as it stands (T001–T011: `Error`,
`ids`, `paths`, `config`, `task`, `state`, `classify`) splits the catalog three
ways, not two:

- **20 entries whose payload types exist today.** Nineteen are in the plan's
  inclusion set; the twentieth is `TddExceptionUsed`, whose `TddException` T011
  just defined, plus `DecisionResolved` (`PathBuf`, `String`) and
  `SelfHealingReport` (`AttemptId`, `FailureClass`, `Vec<String>`, `String`),
  which are named in the exclusion list and stay there.
- **8 entries whose payloads name a type no task has written** — `GateResult`,
  `Usage`, `AttemptRecord`, `Capabilities`, `DecisionRequest`. The plan lists
  these, and they cannot be defined: an enum cannot hold a type that does not
  exist.
- **1 entry the plan forgot: `GateStarted`.** Its payload is `kind: GateKind`,
  and `GateKind` lives in `gate.rs`, which T038 writes. `docs/DESIGN.md` spells
  the enum out under "Core types", so it *looks* available in exactly the way
  `Usage` and `Capabilities` do not, and the exclusion list was written as if
  it were. It is not: `error.rs` already works around the same absence by
  holding `Error::Gate::kind` as a `String` (ADR-0001).

So `GateStarted` satisfies the inclusion rule's *intent* and violates its
letter, and three entries satisfy the letter of the inclusion rule and are
named in the exclusion list. One of the two has to give in each direction.

## Decision

`EventKind` defines 19 entries: the plan's inclusion set, minus `GateStarted`.

`GateStarted` is deferred to T038, the task that writes `gate.rs`. It is not
given a `String` payload in the meantime, and `GateKind` is not defined early
in `event.rs`.

The eight entries the plan defers stay deferred even where their payload types
happen to exist already. A test asserts the nine absences, so an entry arriving
ahead of its producer is a failing test rather than a quiet addition.

## Alternatives considered

- **`GateStarted { kind: String }`, per ADR-0001.** Rejected. ADR-0001 reaches
  for the underlying form because `Error` is named by 160 signatures and cannot
  wait; an event variant can. Nothing constructs a `GateStarted` before T038,
  and `docs/DESIGN.md:206` says a payload may only be added alongside its
  `state::apply` arm. Worse, a `String` gate kind is durable data: journals
  written before T038 would hold whatever spelling the emitter chose, with
  nothing checking it against the seven `GateKind` variants. `Error::Gate`
  accepts that trade because its alternative is a cycle; the journal has no
  such pressure.
- **Define `GateKind` now, in `event.rs`.** Rejected: the same collision
  ADR-0001 rejected. T038's file list is `gate.rs`, so it would begin by
  deleting a duplicate it does not own.
- **Define `TddExceptionUsed`, `SelfHealingReport` and `DecisionResolved` now,
  since their payload types exist.** Rejected: the plan's list is explicit, and
  the deferral is load-bearing rather than clerical. An entry in the catalog
  with no producer and no `apply` arm is an event that *can* be journaled and
  whose effect on state no task has specified — a journal the replay cannot
  reproduce, which is the one failure this project exists to prevent. The task
  that emits each entry adds it in the same commit as the code that emits it.
- **Report NEEDS_INPUT.** Rejected: both directions resolve mechanically once
  the payload types are listed, and neither choice needs a product decision.
  Recorded here instead, as a finding.

## Consequences

The catalog is a strict subset of `docs/DESIGN.md`'s until T038 and the
provider, decisions, remediation and self-healing tasks land. Anything that
reads the journal must treat an unknown `kind` as corrupt data rather than as a
variant it forgot to handle — which it must do anyway, because the file
outlives the binary that wrote it.

Two later tasks owe this file an edit, and both are cheap: T038 adds
`GateStarted` and `GateFinished` with their `apply` arms, and each deferred
entry moves from the test's `DEFERRED` list to its `DOCUMENTED` list in the
commit that first emits it. The counts in that test are the ledger; if the
totals move without a producer landing beside them, that is drift and the test
says so.

`docs/DESIGN.md` should also be corrected: `GateStarted` belongs in the list of
entries awaiting a payload type. That is a documentation task's edit, not this
one.

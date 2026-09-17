# 0025. What makes a generated journal legal is `apply`'s answer, not a second table

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T025 adds the property VISION.md section 15 names and `docs/TESTING.md` calls the
invariant the design rests on: the materialized state must equal the journal's own
projection, so the projection can be thrown away and built again. ADR-0024 made
that rebuild real; nothing so far compared its answer against the projection a
run wrote, except over sequences a person typed out.

The task's own completion check is what forces the decision: *generators only
produce legal sequences*, and a separate test covers illegal ones. "Legal" is a
fact about the transition table in `state::apply`, which is already written once
and already tested exhaustively per state (ADR-0022). The property's generator
has to know that fact to filter what it writes, and there are only two ways to
know it.

Three further pressures shaped the answer:

- **The illegal half has the same problem.** ADR-0016 lets `append` journal a
  record the machine would refuse, and ADR-0024 makes a replay refuse such a
  journal. To test that, a case needs a record `held` will not take — and if the
  test had to say which records those are, it would be the second table again,
  this time proving the test agrees with itself.
- **Depth, not just legality.** Uniformly random `EventKind`s are legal about
  once in a long while: `VerifyPassed` names an attempt, `TaskDone` names a
  commit the remote was proved to hold. A generator that draws payloads blind
  writes journals that die at `Queued`, and a property over "every legal journal"
  silently becomes a property over one row.
- **A claim about range needs evidence.** 256 cases of a generator that favours
  three states prove equivalence over three states. Nothing in proptest says how
  wide the generator actually goes.

Measured before choosing: a walk of blind payloads reaches two states; a walk that
takes the attempt, phase and commit a state already holds and draws only what the
state cannot supply reaches all twelve, and `synchronous = FULL` (ADR-0017) makes
every record its own commit, which is what bounds case cost.

## Decision

**Ask the machine.** A generated step names one of the catalog's nineteen entries
and *aims* it at the state the walk has reached; `apply` decides. A proposal it
refuses is dropped rather than journaled, so every journal the property writes is
one a run could have written. Legality is a consequence of the same function the
production recorder consults, so it cannot drift from it.

**Payloads come from the state wherever the state has one.** The attempt an entry
names is the attempt the task is on, the phase an interruption names is the phase
the task is in, the commit a `TaskDone` names is the one `PublishedVerified`
proves. Dice carries only what no state holds: which entry, which pause reason,
which verdict, which failure class, whether the remote agreed, free text, an
instant. This is what makes the deep states reachable rather than merely
generatable.

**The walk keeps its own answer.** The states it accepted are held beside the
records and compared against both the written projection and the rebuilt one, so
a case fails if either disagrees with the machine — not only with the other.

**A refusal is found, not remembered.** `refused_at` walks the same proposal
functions against the state it is aimed at and returns the first one `apply`
refuses, falling through to a `TaskDone` naming a commit no reading of the remote
proved, which every state refuses.

**Reach is proved, not claimed.** Two deterministic tests walk the machine with
the generator's own aiming functions: every state name must be one a legal walk
arrives at, and every catalog entry must be the refused side of some pair and the
refusal aimable at every state. They fail by name when a future generator narrows.

**256 cases are stated in the file**, through `ProptestConfig::with_cases`, rather
than left to a configuration nobody reviewing the test can see.

## Alternatives considered

**The generator holds a table of what each state accepts.** It is the direct
reading of "generators only produce legal sequences", and it loses because the
table already exists: `state::apply`. A second copy fails the moment the first is
edited, and until then it makes the property tautological — the generator agrees
with the test's own opinion, which is not the claim anyone wanted proven.

**Generate `EventKind`s directly and journal whatever comes out.** Shorter by a
filter, and it fails the second bullet above: the journals are almost all illegal,
so the legal half of the property runs at `Queued`, and the walk cannot know the
state it expects because it never learned which records took.

**Enumerate the legal paths as a grammar** (`Queued → Preflight → Running … Done`).
It is legal by construction with no filter, and it is a third reading of the
transition table that no check keeps honest. It also fixes the shapes in advance:
the walk that found `Remediating` — a `PhaseEntered` naming a *later* attempt — is
a shape nobody would have written into a grammar.

**Seed the illegal half from the existing refusal unit tests.** Those tests name
three pairs, which is what the second property would then run over, and it would
still say nothing about whether a refusal can be aimed at a `Paused` task behind a
human gate.

**Fold the reach check into the property as an assertion.** It would report as a
property failure and shrink to an empty journal, telling the next reader that the
projection and the replay disagreed when the true news is that the generator got
narrower.

## Consequences

- A machine-wide bug inside `apply` is invisible *to these two properties* — the
  generator and the walk would both be wrong the same way. That is deliberate:
  `apply` is what ADR-0022's exhaustive per-state tests own, and what these two
  prove is the fold, the encoder, the row writer and the equivalence between two
  code paths over one file.
- The generator is now a reader of the catalog. A twentieth entry makes
  `PROPOSALS`' length and the `ENTRIES` count disagree at compile time, and an
  entry or state missing from `every_entry_name` / `every_state_name` fails the
  reach tests — so the properties cannot narrow in silence, and the cost of adding
  an entry is one aiming function.
- Case cost is bounded by the journal's length, not the state space: twelve
  records per case at `synchronous = FULL` keeps both properties near a second
  and a half each.
- Two mutants survive *these* properties that the suite as a whole kills — a
  rebuild that refills without clearing is caught by
  `journal::projection::tests::a_row_for_a_task_the_journal_says_nothing_about_is_gone_after_a_rebuild`,
  because a generated journal has a row for every task it mentions and so cannot
  show a stale one. Widening the generator to retire a task is the way to close
  that, and it is not this task's scope.
- `proptest` writes a seed file beside a failing case; nothing is committed here,
  because the seeds this run produced were failures of deliberately broken builds
  used to check that the properties bite, not of the implementation.

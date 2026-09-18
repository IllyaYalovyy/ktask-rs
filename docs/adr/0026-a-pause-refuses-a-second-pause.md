# 0026. A pause refuses a second pause

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T022 settled the state machine in code and recorded its rules in ADR-0022, one
of which was that a second `Paused` event arriving at a `Paused` state nests:
the outer pause boxes the inner one, so the run resumes once per reason. T028
takes pause and resume as its own subject — "a paused task knows exactly where
it resumes" — and its completion check is the opposite: *nested pauses are
rejected*. The two cannot both hold, so the later, narrower task wins and the
earlier rule is withdrawn here rather than left to drift out of the code.

What the documents say about a pause:

- VISION.md §4 fixes five durable pause states (`waiting_limit`,
  `waiting_input`, `human_gate`, `interrupted`, `blocked`), and each names one
  reason a run stands still.
- `docs/DESIGN.md` Core types gives the pause exactly one return address:
  `Paused { reason: PauseReason, resume_to: Box<TaskState> }`.
- `docs/CONTRACT.md` `pause`/`interrupt`/`cancel` and `resolve`: "Each exits 2
  when nothing is in a state the command applies to."

Nesting contradicts all three in the same way. A pause that boxes a pause holds
**two** reasons and **two** return addresses, so "where does this resume" stops
having one answer, and the pair stops being one of the five states VISION.md
says the run is in: `Paused { Limit, Paused { Interrupted, Running } }` is a
state the document never named. The command surface has nowhere to put it
either: `pause` twice on one task would have to succeed, then need two
resumes, and a supervisor that died between them would come back owing a wait
nobody asked for. Recovery is the deciding cost. Fold is the mechanism
(ADR-0024), and a journal whose every `Paused` record is legal in both
directions is a journal whose end state depends on how many redundant waits
were journaled, which is exactly the ambiguity VISION.md §6 says a transition
must not leave.

Refusing is also the only answer `apply` can give without reading anything but
its arguments: the function is pure (ADR-0022) and cannot consult a clock or
`Config`, so it cannot know whether a second limit is "really" a different wait
from the first.

## Decision

`apply(Paused { .. }, Paused { .. })` returns
`Error::InvalidTransition { from: "Paused", event: "Paused" }`, whatever the
incoming `PauseReason` and whatever the pause boxes — including a hand-built
pause over a pause, or over a terminal state, so the rule reads "is this a
pause" rather than "what does it hold". One reason, one return address, one
`Resumed` per pause.

The type still boxes a `TaskState`, because `docs/DESIGN.md` writes it that way
and the journal codec must read what was already written: a nested encoding
still round-trips (`a_pause_nested_in_a_pause_keeps_its_own_resume_state_and_instant`
keeps that proved), it simply cannot be reached by a transition any more.
Changing the shape to make nesting unrepresentable would rewrite every journal
written before this decision, which is the drift `docs/DESIGN.md` Core types
exists to prevent.

`("Paused", "Paused", "Paused")` is removed from the hand-written `LEGAL` table
in `state.rs`, and the table's length is now asserted to be 48 with a note that
a pair leaves it only by a decision written down. The terminal half of T028's
check — a terminal state refuses a pause — needed no change: the four terminal
states already refuse every event (`no_terminal_state_accepts_any_event`,
ADR-0021).

## Alternatives considered

- **Nest, as T022 recorded.** Rejected: it answers "where does this resume"
  with a stack, invents a state VISION.md does not list, and asks for one
  resume per reason when the CLI has one `resume` and no way to name which
  wait it closes.
- **Replace the pause, keeping the new reason and the original `resume_to`.**
  Rejected: it silently rewrites why the run stopped, which is the fact a human
  answers with `resolve`, and a journal replay would then disagree with the
  journal about which reason was recorded.
- **A self-transition: `Paused` on `Paused` changes nothing.** Rejected: the
  state machine reserves self-transitions for events whose fact is already true
  (ADR-0022), and a second pause with a different reason is not already true.
  It would also make `pause` twice indistinguishable from `pause` once, so the
  command surface could not report the mistake it just swallowed.
- **Refuse only when the reason matches, allowing a nested different reason.**
  Rejected: it keeps the two-return-address problem, and `apply` cannot compare
  futures (`PauseReason::Limit { until }`) against a clock it may not read.

## Consequences

- A supervisor that needs to change why it is waiting must `Resumed` first,
  then pause with the new reason; the journal then records both waits and the
  fold lands in one place.
- `pause` and `interrupt` on an already-paused task exit 2 under
  `docs/CONTRACT.md`'s "nothing is in a state the command applies to", which is
  the reading the contract already supports rather than one this decision adds.
- `a_run_can_always_be_stopped` now sweeps the states a run can still be moving
  through, and the one state it excludes is asserted about by
  `a_pause_above_a_pause_is_refused`: nothing was deleted, and the total number
  of assertions over the cross product went up.
- ADR-0022's rule list and consequences now point here, so the two records
  cannot be read against each other.

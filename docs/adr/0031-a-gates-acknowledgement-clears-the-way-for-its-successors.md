# 0031. A gate's acknowledgement clears the way for its successors

- **Status:** accepted (supersedes ADR-0028 on the `Acknowledged` row)
- **Date:** 2026-09-17

## Context

ADR-0028 implemented `state::check_predecessor` over three states —
`PublishedVerified`, `Done`, `Cancelled` — and recorded the fourth as a question
it would not answer alone: "`Acknowledged` is a live question, not an oversight …
That is a product question about how gates sequence, and invariant 8 gives it to
the human." ADR-0029 then pinned the cost from the selector side:
`queue::tests::an_acknowledged_gate_still_holds_its_successors`, with the note
"Whoever supersedes ADR-0028 changes one row of `clears_the_way` and that one
assertion."

T034 is that decision, and it says which way to decide it: "treat `Acknowledged`
as a terminal success in `check_predecessor` and `next_runnable`". The documents
had already argued the same direction twice:

- VISION.md §3 invariant 2 names the states a successor waits on — "`done` for an
  executable task, `acknowledged` for a gate, `cancelled` where a human has said
  so" — so a gate's terminal success is the acknowledgement, not a publication.
- VISION.md §3 invariant 7 says a gate "is not an executable task: it produces no
  commit", so a gate can never reach `PublishedVerified` and reaches `Done` only
  through it (ADR-0021). Under ADR-0028's three states a passed gate therefore
  blocks every later id forever, which is the failure §6 names out loud: "Without
  this distinction a gate could never satisfy invariant 2, and one gate would
  deadlock the rest of the queue forever."
- Invariant 8 is what made it a decision rather than a fix, and the decision is
  now on the record, so nothing here is inferred from a preference.

What the machine already supplied, and what this ADR does not change:
`state::apply` reaches `Acknowledged { by, at }` from exactly one place — a
`GateAcknowledged` handed to a pause that stopped at a human gate
(`from_paused`, ADR-0022, tested by
`only_a_pause_at_a_human_gate_is_acknowledged`). `TaskState::is_terminal` already
counted `Acknowledged` terminal (ADR-0021). What was missing was only the
ordering rule's answer.

## Decision

**Four states clear the way: `PublishedVerified`, `Done`, `Acknowledged`,
`Cancelled`.** One row of `clears_the_way`, and its test table in both modules
that keeps a copy of the list (`state::tests::CLEARED`, `queue::tests::CLEARED`).
The refusal `check_predecessor` returns names all four, so a blocked queue reads
the same way as the rule.

**A gate clears the way on the acknowledgement and never on a commit.** The state
it arrives through is the record of a person having been asked and having
answered: the run journals `Paused { reason: HumanGate }` when it reaches a gate
entry, and `ack` journals `GateAcknowledged { by, at }` against that pause (T097
and T117 own those two emissions). A gate standing at the question — `Queued`, or
`Paused { HumanGate }` — holds every later id, by the ordering check and by the
selector alike.

**`Acknowledged` is a terminal success, not a runnable row.** `next_runnable`
needs no gate-specific branch for the passed gate: it is no longer awaiting a
start, so the selector moves to the work the gate was holding. It also never
names a gate at all, in any state, because a gate is never handed to a provider
(VISION.md §6) — `a_gate_is_never_the_task_the_selector_names` sweeps all twelve
states to keep that true.

**The transition becomes a declared one.** `LEGAL` gains
`("Paused", "GateAcknowledged", "Acknowledged")`, and the sweep state for
`Paused` in `one_state_per_variant` now carries `HumanGate` rather than `Input`,
because that is the one pause whose answer depends on the reason. The refusal for
the other four reasons stays pinned where it already was.

## Alternatives considered

- **Leave ADR-0028's three states in force.** Rejected: it is the reading under
  which the first gate in a plan is a permanent full stop, and invariant 2 names
  `acknowledged` as the state a successor waits on for a gate. This is the
  alternative ADR-0028 itself called "the more tempting wrong answer" for
  deciding it *without* the decision; the decision has now been made, which is
  the only thing that changed.
- **Accept `GateAcknowledged` from `Queued` as well, so `ack` works on a gate the
  run never reached.** Rejected: `apply` is shown a state, never a queue row, so
  it cannot see the `**Gate:**` section that makes an entry a gate. Admitting the
  event from `Queued` would let an `ack` close *any* queued task — including an
  executable one — with no preflight, no attempt and no publication, which is
  exactly what invariant 4 refuses. The pause is what records that a human was
  actually asked.
- **Clear the way on `Paused { HumanGate }` too.** Rejected: that makes asking the
  same as being answered, and the whole value of a gate is that someone replies.
- **`is_terminal()` as the predecessor test now that `Acknowledged` is terminal.**
  Rejected for the reason ADR-0028 and ADR-0029 both give: `is_terminal` is also
  true of `Failed`, so the head would be offered past a failure two rows up. The
  two predicates answer different questions and stay separate.
- **Let `ack` write a status onto the queue row instead.** Rejected: `task.rs` is
  explicit that `TaskStatus` is what a supervisor concluded and that the
  conclusion comes from the journal, and ADR-0019 kept status out of the row.

## Consequences

- **ADR-0028's open finding closes, and one gate no longer deadlocks a queue.**
  `queue::tests::an_acknowledged_gate_lets_its_successors_start` replaces the
  assertion ADR-0029 pointed here, and
  `a_gate_in_the_middle_runs_what_is_before_it_and_stops` tells the whole story in
  one test: work above the gate runs, the gate stops the run, `ack` runs the rest.
- **The runner owes the pause.** `ack` on a gate the run never reached stays
  `Error::InvalidTransition`, which is the right answer for `ack`'s documented
  "exits 2 if no gate is pending" (docs/CONTRACT.md). T097 journals
  `Paused { HumanGate }` on a gate task; T117 issues the acknowledgement. If
  either forgets, the queue stops at the gate rather than skipping it, which is
  the safe direction for a mistake.
- **A documented sentence now reads oddly.** docs/CONTRACT.md says `ack` "leaves
  the queue paused; `resume` continues". The gate's own row is no longer paused
  once it is acknowledged; what stays stopped is the *run*, which stops at the
  gate and exits 4 (docs/CONTRACT.md §1, T129) and is continued by `resume`.
  Reported rather than rewritten, since `docs/CONTRACT.md` is the CLI surface
  another task owns.
- **Two copies of the cleared list moved together, on purpose.** ADR-0028's
  tables exist so neither can pass by agreeing with the implementation; a fourth
  row in one and not the other fails the sweep that reads them.
- Coverage of the sweep shifted rather than shrank: `Paused` is now exercised with
  the reason that carries an event-dependent answer, and the four reasons that
  refuse an acknowledgement were already swept by their own test.

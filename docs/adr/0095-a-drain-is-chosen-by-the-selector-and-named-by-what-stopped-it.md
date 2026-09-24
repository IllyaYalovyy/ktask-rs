# 0095. A drain is chosen by the selector, and named by what stopped it

- **Status:** accepted
- **Date:** 2026-09-24

## Context

VISION.md §3's second invariant says tasks run strictly serially, in id order, and
that a successor cannot start until its predecessor's completion has been proved.
T099 adds the step that has to hold it over a whole queue:
`Runner::run_queue(&mut self, tasks: &[Task], from: Option<TaskId>)`, which drains a
queue and stops at the first thing that stops it.

The parts already existed and none of them decides the drain on its own.
[`queue::next_runnable`] answers the head of a queue given a projection, and refuses
a projection with two active tasks. [`Runner::run_task`] drives one task and returns
`Result<TaskState>`: `Ok(Done)` for a finished task, `Ok(Paused)` for each of §6's
waits, an `Err` for a refusal — and, because §7's bounds have to *end* a task, an
`Err` for a failure whose [`EventKind::TaskFailed`] row was appended first.
ADR-0094 gave the run its seven named answers and took the exit numbers out of this
crate. `docs/CONTRACT.md` §1 adds the rule the drain is judged by: a run "never
continues past a failed task".

Four questions had no answer in the existing code, and each one is a decision some
later task could otherwise quietly reopen.

**Who chooses the next task.** A drain could walk its slice and start whatever it
found waiting. That would be a second copy of the ordering rule, and ADR-0028's
point about [`check_predecessor`] is that an order enforced in one place can be
checked in every place for free.

**What an `Err` meant.** A failed task and a fault of the supervisor's own both
arrive as `Err`, and the two have opposite meanings: one is §1's answer with a name
in it, the other is damage that must not be reported as work. Nothing in the return
value distinguishes them.

**What to do with a gate.** [`queue::next_runnable`] never returns a gate, because
VISION.md §6 says a gate is never handed to an agent. So its silence means both
"nothing left to start" and "a gate is holding the queue", and a drain that read the
first as the second would leave the gate sitting in `Queued` — from which `apply`
refuses [`EventKind::GateAcknowledged`] (ADR-0026), so `ack` could never reach it and
everything behind it would wait forever.

**What `from` means for the ids behind it.** `run --from` and `resume` are the
operator saying *start here* about a queue this run has not finished. Ordering can be
enforced among the ids that were asked for; enforced over the ids that were
deliberately skipped, `resume` would refuse the queue it exists for.

## Decision

**The selector picks; the loop does not.** Each round of `run_queue_with` rebuilds the
queue's projection with [`Runner::projection`] and asks [`queue::next_runnable`] for
the next id. Nothing in the drain reads a state to choose a task, and nothing reaches
past a task it could not start, so the order a drain keeps is the same fact the queue
screen displays and a recovery walk re-checks.

**The projection is folded from events, never read from `task_state`.**
[`Journal::append`] moves events and [`Journal::rebuild_state`] is the separate pass
that recomputes the table, so a drain that read the table would be asking about the
last time somebody else rebuilt it — and the task it started a moment ago would still
look `Queued`, which is how a drain starts one task twice. `folded_in` folds one
task's rows over a journal the drain already holds open.

**The journal decides what an `Err` meant.** After any refusal from
[`Runner::run_task`] the drain folds that task's own rows. Rows that leave it
`Failed`, or parked at a limit, a gate, a question or an interruption, are answered
with the §1 stop they stopped in, through the one mapping [`stopped_on`]. Rows that
leave it nowhere — `Preflight`, a live state — mean the refusal was never about the
task, and the fault comes back untouched. `RunOutcome::Drained` is never reachable
from a refusal.

**A gate is handed to `run_task` like anything else.** When the selector is silent and
the first entry still waiting has a `gate`, the drain runs that task; its only path is
[`Runner::park_at_the_gate`], which journals the `Paused { HumanGate }` row that makes
the gate ack-able and starts nothing. The drain's answer is then
`RunOutcome::HumanGate { task }`.

**`from` narrows the projection as well as the slice.** `drain_from` filters the
entries to the ids at or after it, sorts the result by id, and refuses an id the queue
does not hold with [`Error::NotFound`] — reading an id past the end as "no `from`"
would drain the whole queue under a command that asked for its tail. Every round then
asks the selector about the narrowed set only.

**The answer names the stop and carries no tally.** ADR-0094 fixed `RunOutcome` at
seven variants; how far a drain got is in the journal, which is where a screen reads
it and a replay checks it. A count in the return value would be a second account of
the same facts, free to disagree.

**A queue the drain could not start still answers something.** A row another run is
standing in, or a pause `resume` has not lifted, means nothing further may start in
*this* run: that is [`RunOutcome::Drained`], and the row is what a screen shows. The
one exception is [`PauseReason::Blocked`], §6's pause for something outside the
supervisor, which has no §1 answer of its own; it is reported as
[`Error::Policy`] rather than dressed as a gate or a question.

## Alternatives considered

- **Keep a cursor in the runner** (`self.last_finished`) and start the next id when it
  clears. Rejected: it is memory where the journal is proof, and VISION.md §3's method
  is that a rule which depends on remembering is a rule a crash breaks.
- **Walk the slice and start every waiting task whose predecessor is done.** Rejected:
  two copies of the ordering rule, and the selector's stricter slot probe (ADR-0028
  leaves `PublishedVerified` as the state where the two rules disagree) would drift
  from the copy that decided.
- **Read the `task_state` table** instead of folding events. Rejected: nothing appends
  a row and updates that table in one step, and a drain that re-ran a task it had
  already started breaks invariant 2 on its first round.
- **Map any `Err` to `RunOutcome::TaskFailed`.** Rejected: a lock another run holds, a
  journal that would not open and a checkout git refused would be reported as work
  that failed, and a repair or a retry would be aimed at a task that never ran.
- **Map any `Err` to `RunOutcome::Drained`** ("nothing more could start"). Rejected:
  that is the answer that says the queue finished, which is the one falsehood a
  supervisor must not tell.
- **Park a gate in the drain** — append the `Paused` row here rather than run the task.
  Rejected: the row is the gate task's own step, `park_at_the_gate` already owns it,
  and a drain that writes task rows of its own becomes a second place a task's
  history is made.
- **Refuse a `from` whose predecessors are unfinished.** Rejected: that is what
  `resume` means, and the operator's instruction is the one fact the drain is not
  entitled to overrule. Ordering behind `from` is left to whoever runs the ids above
  it.
- **Give `PauseReason::Blocked` a variant of its own.** Rejected for now: §1 fixes
  seven answers and adding an eighth belongs to whoever owns that table (T105), not to
  the code that met the pause.

## Consequences

- A drain's order is testable from outside: the origin's commit subjects, oldest
  first, are the order seen from behind, and the journal rows per task are the same
  order seen from the side. `mod run_queue` asserts both.
- Stopping is idempotent. A second drain over a queue that stopped at a failure or a
  pause answers the same stop without starting anything to find it, which is what
  makes `run` safe to repeat.
- Every stop is a mapping in one function, so a new §1 answer has exactly one place
  to be added and the compiler lists the states that do not yet mean anything.
- `Error::Policy` for `Blocked` means a CLI meeting that pause has no exit code to
  print until §1's table gains one. That is recorded here rather than invented, and
  belongs to the task that owns the table.
- `from` skipping unfinished work is now the operator's responsibility rather than
  the drain's. If `resume` is later given a `--strict` that checks the ids behind it,
  that check goes in the CLI, above this decision, not inside it.

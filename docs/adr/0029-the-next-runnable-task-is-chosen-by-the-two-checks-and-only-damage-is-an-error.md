# 0029. The next runnable task is chosen by the two checks, and only damage is an error

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T032 adds `queue::next_runnable(tasks, states) -> Result<Option<TaskId>>`, the
one question a drain, a queue screen and a recovery walk all ask: *what may start
now?* ADR-0028 names it as the task that composes the two ordering rules, and the
composition had four real choices in it rather than one:

- **A refusal is not always an error.** `check_one_active` refuses two tasks
  active at once and `check_predecessor` refuses a successor whose predecessor is
  unpublished, but the second refusal is what a healthy serial queue looks like
  most of the time: task 1 is running and task 2 is waiting. Returning that as
  `Err` would make the ordinary state of a running queue an error every caller had
  to handle, and a queue screen would print a fault where it means "wait".
- **The two rules disagree about `PublishedVerified`, and someone has to choose.**
  ADR-0028 records it: the ordering check clears a predecessor the remote was
  proved to hold, while ADR-0027 keeps that same state in the one active slot
  until `TaskDone` closes it. Read alone, each answer is correct; composed, one of
  them has to decide what the next task is, and the choice is visible in behavior.
- **The task body's two lists do not agree with each other.** It asks for the
  lowest `Queued` id "whose predecessors all satisfy `TaskState::is_terminal`" and
  also to call `check_predecessor` rather than reimplement it. `is_terminal` is
  true of `Failed` (ADR-0021) and false of `PublishedVerified` (ADR-0028); the two
  predicates answer different questions, and ADR-0028 already rejected folding
  them. The instruction to call the existing check is the one that survives.
- **"Absent from `states`" has two readings, and one of them makes plans
  unrunnable.** The body treats a task with no projected row as `Queued` "only
  after `TaskQueued` has been recorded for it". `Journal::put_tasks` writes queue
  rows and appends no events, so a freshly imported plan has no rows in the
  projection at all — read strictly, nothing in it is queued, and no plan could
  ever be started. The evidence that a task was queued is therefore its row in the
  queue, which is also the only thing that gave it an id.

What the documents otherwise supply: VISION.md §6 says a gate "is never handed to
an agent" and asks for permission "before the work after it may proceed";
docs/CONTRACT.md says `run` stops at the first terminal failure and never
continues past it; `task.rs` says `TaskStatus` is what a supervisor concluded and
that the conclusion comes from the journal, never from the queue row.

## Decision

**An empty answer and an error are different facts, and only damage is an error.**
`next_runnable` returns `Err` only for what `check_one_active` refuses on the
projection as it stands: two tasks active is a durable record claiming something
no run could have done, and the caller repairs it rather than starting a third
task. Every other reason nothing can start — paused, gated, failed, unpublished
below, slot held — is `Ok(None)`, because each is the queue working as designed.

**A pause or a failure stops the whole selection, wherever it sits.** Both are
asked of every row, not only of the head: a pause is released by `resume`, not by
a successor deciding to go ahead, and a run never walks past a terminal failure.
`Cancelled` is not in the set — a human dropped that task precisely so the queue
could proceed past it.

**The active slot is asked about by starting the candidate, on a copy, and
asking `check_one_active`.** The copy holds the candidate in `Preflight` — the
state a `PreflightStarted` record puts it in, and the first state that occupies
the slot — and the refusal is the answer. So there is one list of active states in
the codebase, ADR-0027's, and the selector cannot drift from it. Where the two
rules disagree, this decides in favour of invariant 1: a queue whose head
published and has not closed holds its successor, because the slot is still held.

**A task with no projected row is `Queued`; a task with no queue row does not
exist.** `tasks` is the domain of ids the function will name; `states` only says
what has happened to them since. This matches the journal's own replay, which
folds a task with no events onto `Queued`.

**A gate at the head answers `Ok(None)`, and a gate the queue has not reached
holds nothing back.** It is marked by its `**Gate:**` section, not by
`TaskStatus::HumanGate`, and the answer is "nothing" rather than the task behind
it: offering that task would be the gate bypass VISION.md §6 exists to prevent.

## Alternatives considered

- **`is_terminal()` as the predecessor test, as the task body words it.**
  Rejected: it is true of `Failed`, so the head would be offered past a failure
  two rows up, and false of `PublishedVerified`, so a published predecessor would
  block. ADR-0028 rejected the same folding for the same two reasons; the body's
  own instruction to call `check_predecessor` rather than reimplement it is the
  half that survives, and `is_terminal` is used only where it is the right
  question — never here.
- **Propagating `check_predecessor`'s `Error::Policy` too.** Rejected: it makes
  "task 1 is still running" an error condition in every caller, and the exit code
  a script should see for a busy queue is not a failure code. `run` reaches its
  exit codes by asking what stopped it, which a refusal to answer cannot say.
- **A private `is_active` list in `queue.rs`, checked against the candidate.**
  Rejected: it is a second answer to ADR-0027's question and would drift the first
  time a state is added. The probe costs one `BTreeMap` clone per decision and
  keeps one owner.
- **A public `TaskState::is_active`, beside `is_terminal` and `is_paused`.**
  Rejected for ADR-0027's reason: the probe needs no new public predicate, and a
  public one is a promise the state module would then owe forever.
- **Treating a task absent from `states` as never eligible.** Rejected: an import
  appends no events, so every freshly imported plan would answer `Ok(None)` and
  nothing could ever run. The rejected reading is exactly the one the strict
  sentence in the task body suggests, which is why the decision is recorded here
  rather than left in a code comment.
- **Offering the task behind a pending gate.** Rejected: VISION.md §6 makes a gate
  the thing that work waits for, and `check_predecessor` refuses what is below an
  unreached gate anyway, so the choice was between "nothing" and an error.
- **Reading `TaskStatus::Failed` / `HumanGate` off the queue row.** Rejected:
  `Journal::tasks` reports `Pending` for every row because there is no stored
  status to report, so a row that disagrees with the projection is stale by
  construction. The projection decides; `task.rs` says so.

## Consequences

- `next_runnable` is the first caller of `check_one_active` and
  `check_predecessor`, which is what turns ADR-0027's and ADR-0028's invariants
  from functions a test proves into the rules the next step is chosen by. Invariant
  1 stops being only a check on the projection and becomes a rule about what may
  be started.
- **A queue whose head has published but not closed has no next task.** That is
  the cost of choosing invariant 1 over ADR-0028's clearance at
  `PublishedVerified`, and it is the right cost: the gap closes as soon as
  `TaskDone` lands, one event later, and in the meantime the alternative was two
  runners.
- ADR-0028's open question about gates survives intact: a gate left in
  `Acknowledged` still blocks what is below it, and
  `queue::tests::an_acknowledged_gate_still_holds_its_successors` pins that as
  behavior. Whoever supersedes ADR-0028 changes one row of `clears_the_way` and
  that one assertion.
- `Ok(None)` now has five distinct causes — drained, paused, gated, failed, busy —
  and the function does not say which. `run` needs the difference for its exit
  code (docs/CONTRACT.md §1) and will decide it from the same states; if that
  turns out to want one owner, the answer is a small enum returned beside the id,
  which is a change of shape rather than of behavior.
- Ordering is by id alone, as ADR-0028 left it. When DAG predecessors arrive, the
  predecessor question changes and the slot probe does not.

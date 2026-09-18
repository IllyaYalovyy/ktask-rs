# 0027. An active task has started and has not stopped or finished

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T029 adds `state::check_one_active`, the mechanical form of VISION.md §3
invariant 1: "Exactly one task is active at a time; parallel execution does not
exist in v1." The invariant is quoted; the word it turns on is not defined
anywhere. `TaskState` has twelve variants (ADR-0021 lists which are terminal,
ADR-0026 which is paused), and any subset of them could be "active". The answer
is load-bearing in two directions at once:

- Too wide, and the check fires on ordinary queues. `check_one_active` is called
  over the projection of *every* task, not the runnable ones, so counting
  anything that has not begun work makes a freshly imported plan illegal before
  a single agent has run — and the drain has nothing left to start.
- Too narrow, and the check passes while two agents hold the tree. The state a
  task is in *while an agent works* is the obvious one to count, and it is not
  the only state in which two of them conflict: two tasks verifying have two
  attempts' output going into one set of gates, and two publishing have two
  candidate commits racing for one push.

The task body's own phrasing — reject "more than one … non-paused active state"
— reads as if excluding `Paused` were the whole definition. It cannot be: read
literally, "non-paused" includes `Queued` and the four endings, so a queue of
forty queued tasks and one running one would be refused. The phrase is therefore
a constraint on the definition, not the definition.

What the documents supply:

- VISION.md §2 Non-goals: "No parallel task execution (strict serial is the
  default and the only mode in v1)". Invariant 1 is that sentence made
  checkable.
- `docs/CONTRACT.md` `run`: "Drains the queue in order, strictly serially", and
  stops at a pause with exit 3, 4 or 5. Those exit codes are pauses, and they
  are explicitly *not* failures.
- `docs/DESIGN.md` Core types fixes the twelve variants; ADR-0021 settled
  `is_terminal` (Done, Acknowledged, Failed, Cancelled) and ADR-0026 settled
  `is_paused`.

## Decision

A task is **active** in exactly six states: `Preflight`, `Running`,
`Remediating`, `Verifying`, `Publishing`, `PublishedVerified`. It is the
complement of the two answers already decided: not finished (ADR-0021), not
standing still (ADR-0026), and not merely asked for (`Queued`, which is where an
import lands and where a task waits its turn).

`check_one_active(states)` returns `Error::Policy` when two or more tasks are in
those six states, and the error **names every active task**, in id order, rather
than the first two. The ask describes the two-task case, where both readings
agree; with three active tasks, reporting two would hide the one whose worktree
the next step opens, and would leave the reader to discover the third by running
the check again. Id order is `BTreeMap` order, which is also what makes the
message deterministic enough to assert verbatim.

No path is listed in the `Policy` variant, following `task::validate`: a row of
the queue broke the rule, not a file.

## Alternatives considered

- **"Anything not paused", as the task body reads.** Rejected: it counts
  `Queued`, so every queue with more than one waiting task — every queue — is
  illegal at import, and `run` could never start the second task of a plan.
- **`Running` and `Remediating` only: the states with an agent in front of
  them.** Rejected: it is the reading that lets two tasks into `Verifying` or
  `Publishing` together, which is the damage invariant 1 exists to prevent. An
  attempt whose gates are running and an attempt being pushed are still work in
  flight on one machine, one tree and one remote.
- **Count `Paused` as active, since it has started.** Rejected, and this is the
  decision worth recording. A pause is how a run gives the machine back: exit
  codes 3, 4 and 5 stop the drain and leave a human to answer. If a paused task
  held the active slot, then `check_one_active` on a queue stopped at a gate
  would name the paused task as the one that is running — the wrong diagnosis of
  the only queue state where nothing is running. Whether a paused task blocks
  the *next* task is a separate rule, and T032 decides it separately
  (`next_runnable` answers `Ok(None)` when any task is paused); keeping the two
  questions apart is what lets each say one thing.
- **`pub fn TaskState::is_active`, beside `is_terminal` and `is_paused`.**
  Rejected for now: nothing outside this module asks the question yet — T032
  names `check_one_active`, `is_terminal` and `is_paused` — and a public
  predicate is a promise. The helper is private and exhaustive-match-shaped, so
  a thirteenth state is placed on one side of the line at compile time rather
  than defaulted onto it (the reasoning of ADR-0022).
- **Name the first two active tasks and a count.** Rejected: it is the same
  sentence with a number to reconcile, and the number is what a reader would
  otherwise have to trust.

## Consequences

- `check_one_active` is re-exported from the crate root beside `apply`. `state`
  is a private module, so a `pub` item no `pub use` reaches is unreachable
  (`unreachable_pub`) and dead code to the non-test build; the one-line re-export
  is what makes the task's `pub fn` a promise the crate actually keeps.
- `state::is_active` is the single definition, and the `ACTIVE` table in
  `state.rs`'s tests writes the six states out a second time, in the order
  ADR-0021's `TERMINAL` table uses, so a rename cannot pass by matching itself.
- A queue stopped at a pause passes this check. Blocking the successor on a
  pause is `next_runnable`'s job (T032), so the two rules must be read together
  until that lands; a queue with one paused task and one queued task is legal
  here and runnable nowhere.
- `PublishedVerified` counting as active means a task that has been read back
  from the remote but not yet closed with `TaskDone` still occupies the slot.
  That is intended, and it is also the reason T031 (Done only from
  `PublishedVerified`, with the commit matching) has to hold: it is what keeps
  that last state from being sat in.
- `check_one_active` is not yet called by anything: T032's `next_runnable` is
  written to call it rather than reimplement it. Until that task lands,
  invariant 1 is a function a test proves, not a gate the runner enforces.

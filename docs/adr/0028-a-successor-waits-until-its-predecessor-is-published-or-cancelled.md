# 0028. A successor waits until its predecessor is published, closed, or cancelled

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T030 adds `state::check_predecessor`, the mechanical form of VISION.md §3
invariant 2: "A successor cannot start until its predecessor has reached a
terminal success state: `done` for an executable task, `acknowledged` for a gate,
`cancelled` where a human has said so." The task body asks for a predicate
"requiring every lower id to be `Done`, `Cancelled` or `PublishedVerified`", and
its completion check names that list, so the two sources offer three states each
and agree on only two of them. The task's own `Refs` line says the task body
replaces the invariant's wording, which settles which list to implement — but not
which facts the list has to answer, and those are what made this a decision:

- **`PublishedVerified` carries the invariant's name.** Invariant 7 fixes what
  "published" means: local verification, a clean publication, and fetched
  remote-mainline equality. `PublishedVerified` is the one state whose payload is
  the commit "the remote was proved to hold", and T031 makes `Done` reachable only
  from it, with the commit matching. A list of *finished* states alone would check
  something weaker than the invariant's title: the earliest state at which a
  predecessor's work is proved at the remote is `PublishedVerified`, and a check
  that refused it would block a successor over work that is already, in the only
  sense §3 admits, published.
- **`Acknowledged` is a live question, not an oversight.** `Acknowledged` is a
  gate's ending, and invariant 7 says a gate is not an executable task: it
  produces no commit, so it can never reach `PublishedVerified`, and `TaskDone`
  closes published work rather than a gate (ADR-0021, `state::apply`). Under the
  three-state list a gate left in `Acknowledged` therefore blocks every later
  task permanently. That is a product question about how gates sequence, and
  invariant 8 gives it to the human. The predicate implements the list it was
  given and this ADR records the gap; it does not close it.
- **Two rules already disagree about one state.** ADR-0027 counts
  `PublishedVerified` as *active*, so a predecessor sitting in it clears this
  check while still holding the one active slot. Read alone, each answer is
  correct; read as the queue's only two ordering rules, a head that published and
  has not yet closed lets its successor start by one rule and forbids it by the
  other. T032's `next_runnable` is the task that composes them.
- **"Predecessor" needs a definition, and only one exists in v1.** Ordering is
  the queue's own: `TaskId` is the task's 1-based position, and VISION.md §2
  non-goals rules out parallel execution, with DAG-based ordering named as
  backlog. So the predecessors of an id are the ids below it — but whether the
  check also consults the successor's own row and the rows above it is left open
  by both documents, and answering "yes" there makes an ordinary queue illegal.

What the documents otherwise supply: `docs/CONTRACT.md` `run` "drains the queue in
order, strictly serially" and stops at a terminal failure, which is why a
predecessor in `Failed` is not cleared work; `docs/DESIGN.md` fixes the twelve
variants ADR-0021 and ADR-0026 already partitioned twice.

## Decision

**A predecessor is every id strictly lower than `next`, and nothing else.**
`next`'s own row and the rows above it are never consulted — otherwise every
freshly imported plan, which is rows of `Queued` with nothing started, would have
no startable head at all.

**Three states clear the way: `PublishedVerified`, `Done`, `Cancelled`.** The test
table writes that list out a second time beside the twelve variants, so a rename
cannot pass by agreeing with itself. Everything else blocks, including `Paused`
(work standing still is not work published, whatever it would resume into) and
`Failed` (the drain stops at a terminal failure rather than walking past it). The
predicate is a private exhaustive match, so a thirteenth state is placed on one
side of the line at compile time rather than defaulted onto the permissive side —
the reasoning ADR-0022 applies to `apply`.

**The question is positional, not row-bound.** `next` need not be a key of the
map: the answer is about what lies below the id, which is what lets T032 ask about
a candidate before its row exists.

**The refusal is `Error::Policy`, naming every uncleared predecessor in id order
and opening with the task it refused** — `task 4 cannot start: its predecessors
(task 2 (Failed), task 6 (Paused)) …`. With three unpublished predecessors the
first named is the one the next step waits on, and a reader should not have to run
the check again to learn about the other two. No path is listed: a row of the
queue broke the rule, not a file (ADR-0027, `task::validate`).

## Alternatives considered

- **The invariant's literal three — `Done`, `Acknowledged`, `Cancelled`.**
  Rejected: the task body replaces that wording, and dropping `PublishedVerified`
  would block a successor on a predecessor whose commit the remote was already
  proved to hold, which is the state the invariant is *about*.
- **The union of both lists — add `Acknowledged` as well.** Rejected as the more
  tempting wrong answer, and recorded here precisely because it is tempting: it
  unblocks gate sequences today, and it also decides, without the human, that a
  gate the queue never proved published lets its successor start. Invariant 8 makes
  that a design decision, so it belongs in an ADR of its own with a task to match.
- **`is_terminal()` as the test, with `PublishedVerified` added.** Rejected:
  `is_terminal` is true of `Failed` (ADR-0021), so the union reads "finished or
  published" — the reading that lets a successor start beside an unanswered
  failure. The two predicates answer different questions and must not be folded.
- **`Error::NotFound` when `next` is not a row of `states`.** Rejected: it makes
  the check partial for no gain, and T032 asks about candidates it has not yet
  materialised. A queue that holds nothing answers "nothing below you is
  unpublished", which is true and is what the drain needs at the head.
- **`pub fn TaskState::is_cleared`, beside `is_terminal` and `is_paused`.**
  Rejected for ADR-0027's reason: nothing outside the module asks the question yet,
  and a public predicate is a promise.

## Consequences

- `check_predecessor` is re-exported from the crate root beside `apply` and
  `check_one_active`: `state` is private, so a `pub fn` no `pub use` reaches is
  unreachable and dead to the non-test build.
- **A gate left in `Acknowledged` blocks its successors.** Reported as a finding.
  Unblocking it needs a decision about how gates sequence (invariant 8), after
  which the change is one line of `clears_the_way`, one row of the `CLEARED`
  table, and the ADR that supersedes this one.
- A queue whose head is in `PublishedVerified` passes this check and fails
  `check_one_active` the moment the successor starts, because ADR-0027 keeps that
  state in the active slot until `TaskDone` closes it. The two rules must be read
  together until T032 composes them, and `PublishedVerified` is the state where
  they differ.
- `check_predecessor` is not yet called by anything: T032's `next_runnable` is
  written to call it rather than reimplement it, and is also the task that decides
  whether a `Paused` task halts the drain (ADR-0026). Until then invariant 2 is a
  function a test proves, not a gate the runner enforces.
- Ordering is by id alone. A plan whose real dependencies are not its queue order
  cannot be expressed in v1; DAG predecessors are the backlog item VISION.md §3
  pairs with a non-strict mode, and this predicate is the one that changes shape
  when that lands.

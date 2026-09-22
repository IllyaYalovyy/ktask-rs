# 0084. A prepared task is journaled before its lock is taken, and its checkout is cut from the fetched tip

- **Status:** accepted
- **Date:** 2026-09-22

## Context

T089 hands over one sentence of shape — a `prepare` that "records
`PreflightStarted`, calls `preflight`, records `PreflightPassed` or
`PreflightFailed` with the class, acquires the repository lock with
`RepoLock::acquire`, and creates the worktree with `git::create_worktree` from the
fetched remote SHA", returning a value holding "the worktree path, the base SHA and
the held lock". Five verbs, and each already has an owner elsewhere that gets to
contradict the order they happen in:

- ADR-0082 deliberately handed the verdict row back *unwritten*: `preflight`
  returns it through `PreflightReport::event()` "for the caller that owns the run's
  recorder to append", because `PreflightFailed` is the row that ends a task and a
  second writer appends that decision twice. T089 is that caller, and it is the
  first one to exist.
- The same ADR puts the lock among the five checks, and that check's own passing
  answer is "the repository lock … was taken and given back". So one step is asked
  both to test a lock by releasing it and to hold it afterwards.
- `lock::acquire` takes a `Duration`, and `Config` has no lock timeout in it —
  `attempt_timeout_secs` bounds an attempt, not a wait. Any wait here is invented
  by this function rather than configured by anybody.
- `git::create_worktree`'s `name` *is* the directory (ADR-0043: one name, one
  directory, in the repository's managed `.ktask-worktrees` beside the tree), and
  VISION.md §7 requires a remediation to "continue in the same worktree". The name
  therefore decides whether a retry finds the work it is told to read.
- `EventKind`'s catalog has no row for a checkout, and `PreflightReport::base_sha`
  is empty until the mainline check has passed — so a report that refused has no
  commit to hand a creation call even by accident.

## Decision

**`prepare` is the writer of both rows, and of no others.**
`EventKind::PreflightStarted` is appended before the checks are asked, and
`report.event()` — unmodified — is appended after they answer. The class the
refusing check named is the class the journal holds; nothing here re-derives it, and
`crate::classify()` is never asked to guess what a check already said (ADR-0057).
No row is invented for the checkout, because the catalog has none and this task is
not the place to widen it.

**A refusal is `Error::NotFound`, not a new variant.** The signature promised a
`Prepared` and the world refused to supply one, which is what that variant means;
`recorded_base` in the same file already refuses a task no `PreflightPassed` ever
gave a base, for the same reason. The message carries the check that refused, the
class it named, and every line of the report's evidence, because the row is written
for recovery while the message is what an operator or a caller reads first.
`error.rs` is a closed catalog of what can go wrong, and a fourth way of reporting
a refusal it can already say is not a new fact.

**The lock is taken after the verdict, with `Duration::ZERO`, and then held.**
After the verdict because a refusal has no business holding a lock nobody is about
to need — and because taking it first makes the refusal itself ambiguous: the row
would say a task was refused while the file said this run was busy, which is the
one thing VISION.md §6's "never silently re-runs work that may already have taken
effect" forbids. With no wait because nothing configures a wait, and because
waiting is recovery's decision made from the class this step has just journaled:
the lock check names `EnvironmentFailure`, whose recovery is to ask again later, not
to sit here. Held, rather than taken and given back like the check does, because
VISION.md §10's step 5 publishes under this lock, and what makes "the checkout was
cut from the fetched tip" still true when the candidate is pushed is exactly this
hold.

**The checkout is cut from `report.base_sha`, never from `git::head_sha`.** The
report's base is the tip `<remote>/<branch>` had when the fetch moved it; the
working checkout's tip is a fact about wherever the user left their tree. VISION.md
§10's step 2 names the first, and a task cut from the second would be verified
against a commit nobody fetched.

**One checkout per task, named `task-<id>`.** Not per attempt: VISION.md §7's
remediation continues in the checkout that stopped, and ADR-0043 makes a name a
directory, so an attempt number in the name would orphan the work whose evidence
the next attempt is told to read. `task-7` is one path component, which is all
`create_worktree` accepts, and it is the shape ADR-0043 anticipated.

**`Prepared` holds the lock in a private field with no getter.**
`RepoLock::release` consumes the lock, so a public field would let a caller move the
lock out or drop it early while the run was still publishing under it — turning the
type's one guarantee back into a call somebody remembered. `Prepared::reclaimed` is
the one accessor, and it exists because a takeover must not be silent (the module
is explicit): the reason a lock is left behind is usually the reason the work before
it did not finish. `prepare` is `pub` for the same reason `begin_attempt` is
(ADR-0083): the queue walk that calls it is a later task, and the alternative to
`pub` is inventing a caller to keep a visibility private.

## Alternatives considered

- **Letting `preflight` append its own verdict.** It cannot: it has no recorder,
  and ADR-0082 gave the row back on purpose. A second writer ends a task twice.
- **A `Error::Preflight` variant, or a `Refusal` type in `Error`.** A fourth shape
  for "the answer was no" buys nothing that the variant in use does not already
  carry, and every caller matching on `Error` today would need a new arm to keep
  saying the same thing.
- **Taking the lock before the checks, to close the race the check leaves.** The
  check's answer is advisory by construction — it gives the lock back — so a second
  `acquire` is what actually decides, and it is still here, after the checks. The
  ordering is not a race that was lost; it is the difference between a refusal that
  leaves the machine free and one that leaves it held by a run that just turned the
  task away.
- **`Result<Prepared, Refusal>` or a `bool` in the success path.** A refusal is not
  a value the caller wants to keep; it is the absence of the thing the function
  promised. `Error::NotFound` is that sentence in this crate's vocabulary.
- **A `timeout` from the attempt budget.** `attempt_timeout_secs` bounds an
  attempt. Borrowing it to bound a lock wait would spend the attempt's time waiting
  for a lock, and would make the attempt's own deadline mean two things.
- **`task-7-a3` — one checkout per attempt.** It reads tidier in
  `git worktree list` and it breaks §7: the remediation would be pointed at a
  directory it never wrote, while the one it wrote rots unregistered.
- **A `lock: Option<RepoLock>` field, or a `lock()` accessor.** Both admit a state
  the task forbids — a prepared task whose lock somebody took — and the second one
  hands out a `&RepoLock` whose `release` needs ownership.

## Consequences

- A step that wants the repository lock after this one must be given the `Prepared`
  rather than call `acquire` itself: `lock::acquire` refuses a second holder from
  the same process, and the refusal message names this very pid (asserted in
  `runner::prepare`). Publication therefore takes its lock from the value that cut
  the checkout, and the lock leaves when that value is dropped — which is what makes
  "published under the lock" a fact about a scope.
- The lock is held across the agent session, which for a long attempt is a long
  hold. `lock` already reports an abandoned holder rather than waiting behind a
  dead process forever, and `Duration::ZERO` means a second run learns that
  immediately instead of after a wait nobody configured.
- Asking twice for the same task hands back the same checkout with its uncommitted
  files untouched (ADR-0043's reuse). The edge that buys: if the mainline moves
  between the two calls, the new base is a commit the existing checkout does not
  contain, and `create_worktree` refuses it as `Error::Policy` naming the directory.
  That refusal is correct and is git's, not this step's, and the recovery is
  `remove_worktree` — a lifecycle question for the task that owns the worktree's
  end, noted here rather than answered.
- Two `prepare` calls for one task append two `PreflightStarted`/verdict pairs.
  That is the honest journal — the checks really ran twice — and the lifecycle
  table, not this function, is what decides a task in `running` is not prepared
  again.
- `Prepared::reclaimed` has no reader yet. It is published rather than kept private
  because the alternative is a private field no test can read — and no lint
  suppression is on the table. The task that files an attempt's evidence should
  carry a takeover into it; if that never happens, this accessor is the one small
  piece of unused surface this task leaves.
- The checkout's own existence is deliberately unjournaled. When a later task wants
  the journal to say a task has ground under it, the honest way is a new
  `EventKind` with fields and a catalog row — not a `detail` string shaped like one.

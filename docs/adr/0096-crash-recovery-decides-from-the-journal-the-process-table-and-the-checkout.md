# 0096. Crash recovery decides from the journal, the process table and the checkout

- **Status:** accepted
- **Date:** 2026-09-24

## Context

VISION.md §6 makes crash recovery a feature rather than an edge case: "on restart it
inspects the live process table, the worktree, and the last persisted transition, then
either resumes the in-flight phase or marks the attempt `interrupted`. It never guesses,
and it never silently re-runs work that may already have taken effect." T100 adds the
step that does it: `reconcile(&mut Journal, &Project) -> Result<Vec<RecoveryDecision>>`.

Nearly every part was already in place, and none of it decides on its own.
[`Recovery`] holds exactly §6's three answers, and [`EventKind::RecoveryDecision`] is
the catalog row that carries one with its evidence. [`apply`] already rules on which
answer each state may accept: [`TaskState::Queued`], [`TaskState::Preflight`],
[`TaskState::PublishedVerified`] and [`TaskState::Paused`] refuse
[`Recovery::MarkInterrupted`], and a phase still in flight parks above itself with
[`PauseReason::Interrupted`] when it accepts one. [`AttemptStarted`] carries the `pid`
whose existence is the question §6 asks of the process table, [`PublishStarted`]
carries the `candidate_sha` that says which commit was already in the air,
[`git::list_worktrees`] answers what the repository holds, [`lock::life_of`] is this
build's only implementation of "may I ask that pid anything", and
[`Journal::rebuild_state`] is ADR-0023's repair for a projection nobody need trust.

Five questions had no answer in the existing code.

**What recovery reads as the state.** `task_state` is a table a run writes; the events
are what the run meant. ADR-0023 made the former disposable precisely so that a crash
could be answered from the latter.

**Which phase may be resumed once its process is gone.** The phases are not alike:
[`Phase::Verify`] and [`Phase::Publish`] are the supervisor's own deterministic work,
while an implementation phase is an agent session that no longer exists and cannot be
re-attached. Resuming either one the same way would either throw away work that is
simply re-runnable or restart an agent whose session is the thing that was lost.

**Who repairs the projection, and how that is recorded.** §6 wants a decision for every
conclusion, and a rebuilt table is a conclusion — but it belongs to no task.

**What an impossible combination means.** A checkout with no journal behind it, a
commit in a worktree that the journal never offered, an attempt number no
[`AttemptStarted`] row ever claimed, and two live attempts at once are all states no
run could have produced. VISION.md §6: "it never guesses".

**How much a `pid` proves.** [`lock.rs`] needs a start tick to tell a reused pid from
the process that wrote the lock file. [`AttemptStarted`] carries no tick, and adding one
changes durable data and the event catalog.

## Decision

**Three inputs, read in that order and only those.** `reconcile` folds the events with
[`Journal::replayed_states`], asks [`lock::life_of`] about the `pid` the journal named,
and asks [`git::list_worktrees`] about the checkout [`worktree_name`] names for the task
— one `git worktree list` for the whole queue. It never fetches, commits, pushes,
creates or removes a worktree, and never takes the repository lock: recovery decides
what happened, and the commands that change the world are the ones that already own
those side effects.

**The fold is the state; drift between the two is itself a decision.** A task whose
folded state and projected row differ (including a row for a task the events say nothing
about) is reported once, before any per-task verdict, as
[`Recovery::AlreadyApplied`] on the queue (`task_id` `NULL`): the journal's answer had
already been applied, and the table was the thing out of date. The row is appended
*before* [`Journal::rebuild_state`] runs, because §3's discipline — the transition
before the side effect — applies to a repair as much as to a push.

The rebuild itself runs on one condition: the projection, as it stands after the
verdicts, is not the map the events now fold to. That catches the case a row-by-row
comparison cannot see — a projection with rows missing, which a crashed run leaves
whenever it died before its first `put_state` — and it leaves a projection that
already agrees alone, so ADR-0023's `updated_at` still tells a repaired row from one a
run wrote. What a caller reads from [`Journal::all_states`] after `reconcile` returns is
the fold, always.

**Machine phases are resumable after their process dies; agent phases are not.** That
one line decides most of the table. `Preflight`, `Verifying` and `Publishing` are the
supervisor's own re-runnable work, so recovery resumes them: the checks run again, the
gates run again, nothing was lost. `Running` and `Remediating` are an agent session,
and when that pid is gone the honest answer is [`Recovery::MarkInterrupted`], which
parks the task above the phase it stopped in and leaves the next session to `resume`.
`PublishedVerified` is already proved and needs only its own `TaskDone` row, so
recovery applies nothing; and a `Paused` task — including one §6's `Interrupted` row
parked — is already waiting in a recorded place, so recovery applies nothing there
either. Lifting a pause is `resume`'s command, not a crash verdict: `apply` would let
recovery un-park a task, and this ADR forbids it from asking.

**A live pid is adopted before anything else is asked.** A `pid` the machine still
answers for means a run is in progress; recovery resumes the task and touches nothing,
because invariant 1 forbids a second one beside it. The same answer stands whether the
checkout is there, gone, or never made: a live process owns its own tree. After the
verdicts are computed but before a single row is written, `reconcile` re-checks
[`check_one_active`] over the states its own decisions would leave — two resumable
tasks is the contradiction, and a recovery that could not see it would start both.

**Publication is decided by the commit, because that is what may already have taken
effect.** With the attempt's process gone, the checkout's `HEAD` is compared against
the journal: `HEAD` equal to [`PublishStarted`]'s `candidate_sha` means the commit
exists and is answered [`Recovery::AlreadyApplied`] (never commit it twice); `HEAD`
still equal to [`AttemptStarted`]'s `base_sha` means the commit never happened and is
answered [`Recovery::Resume`]; any other `HEAD` means the repository holds a commit this
journal never offered, and is [`Error::Policy`]. A checkout that is gone — including
one git reports as prunable because its directory has — cannot be read at all, so the
attempt is marked interrupted rather than guessed about, in the one case where
[`Recovery::MarkInterrupted`] is chosen for a machine phase: it is a per-task record, so
one damaged checkout does not stop the rest of the queue being reconciled.

**An unreachable combination is an error, and a refusal writes nothing.** A checkout
beside a task the journal never started, an attempt-holding state whose attempt no
[`AttemptStarted`] row claims (reachable, because [`apply`] lets [`EventKind::PhaseEntered`]
name an attempt no row introduced), a `pid` of 0, and a terminal state handed to the
decider are each an [`Error`] — `Policy` when the repository contradicts the journal,
`Corrupt` when the journal contradicts itself. Every verdict and every state is computed
before the first append, so a refusal leaves the journal exactly as it was found.
Pid 0 is refused before it is probed, for the reason [`lock.rs`] gives: signalling 0
means every process in this process group.

**`OutOfReach` counts as not-resumable.** A pid that exists but answers only to another
user cannot be adopted, so it is decided with the process that is gone; the difference
is not lost, because the reason goes into the decision's `detail`.

**Every verdict carries its evidence.** The `detail` of a
[`EventKind::RecoveryDecision`] row names the state, the last row the journal held for
that task, the process clause with its pid, and the checkout clause with its path and
what `HEAD` was found there. A later reader can disagree with one line of a recovery
rather than with the whole pass.

**Four visibility widenings, no behavior change.** [`lock::life_of`] and
[`lock::Life`], [`Journal::replayed_states`], `git::managed_dir` and
`runner::worktree_name` became reachable inside the crate so recovery can use the
existing answer rather than a second copy of it. A second `kill(pid, 0)`, a second fold,
or a second spelling of `task-7` would each be a rule that could drift from the one that
decides.

## Alternatives considered

- **Read `task_state` and decide from the table.** Rejected: nothing appends an event and
  updates that table in one step, so the answer would be a report on whenever someone
  last rebuilt it — and a task started a moment ago would still look `Queued`.
- **Re-run whatever phase the journal ends in.** Rejected: it is §6's one prohibition.
  Re-running `Publishing` makes a second commit; re-running an attempt spends tokens on
  work whose evidence may be on disk.
- **Mark every dead-pid phase interrupted.** Rejected: it discards re-runnable
  supervisor work and turns every crash into a human action, which is the opposite of
  §6's "a restart resolves an interrupted run to a known state".
- **Answer `Error::Policy` for a vanished checkout instead of marking it interrupted.**
  Rejected: an error aborts the whole pass, so one lost directory would leave every other
  in-flight task unresolved. A refusal to resume *that* task is the per-task answer.
- **Un-park `Paused { Interrupted }` so the run continues unattended.** Rejected: the
  pause is the record that says where to resume from, and `resume` is the command that
  lifts it. Recovery lifting it would start an agent session on the strength of a crash.
- **Add a fourth `Recovery` variant for "adopt the live process".** Rejected: `Recovery`
  is durable data whose three variants `docs/DESIGN.md` fixes, and adoption *is*
  [`Recovery::Resume`] — §6's "resume the in-flight phase".
- **Record a start tick in [`EventKind::AttemptStarted`] so a reused pid is detectable.**
  Rejected for this task, not on the merits: it changes durable data and the event
  catalog, which is a bigger decision than a reconciliation pass. It is a known hole
  (below), and [`lock.rs`] already demonstrates the shape of the fix.
- **Let `reconcile` prune a checkout whose task is gone.** Rejected: cleanup is a
  command an operator starts; recovery that deletes trees would be a side effect bigger
  than any it was called to decide about.

## Consequences

- A supervisor that died at any boundary resolves to a state the queue can act on, and
  `reconcile` is idempotent: a second pass over a task it parked finds
  `Paused { Interrupted }` and answers [`Recovery::AlreadyApplied`] without moving it,
  so the state never oscillates however often a run reconciles at start-up.
- A recovery pass is auditable row by row: the decisions are in the journal beside the
  run's own transitions, including the pass that repaired the projection.
- Pid reuse inside the window a crashed supervisor left is **not** detectable from
  [`AttemptStarted`] alone, so recovery can report `Resume` about a stranger process.
  Closing it needs the start tick the event does not carry, which is recorded here
  rather than fixed in passing.
- A project whose root holds no repository is refused with [`Error::Git`] rather than
  reconciled: the checkout is one of §6's three inputs, and answering every in-flight
  task `MarkInterrupted` because the question could not be asked would be a guess.
- A worktree somebody locked is read like any other: `locked` says it may not be
  removed, which says nothing about whether a phase may resume.
- `reconcile` is not yet wired into `run`/`resume` or the TUI's start-up path —
  dispatch belongs to the task that owns those commands. Until it is wired, the
  reconciliation is available and tested, not automatic.
- Recovery leaves a `run.lock` held by a dead pid to [`lock::acquire`], which reclaims
  it and reports the reclamation (ADR-0047); a second account of the same abandoned
  lock would be two answers to one question.

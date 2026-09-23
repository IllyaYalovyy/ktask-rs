# 0090. A run writes its own verify entry, gates what its session claimed, and keeps a checkout that holds work

- **Status:** accepted
- **Date:** 2026-09-22

## Context

T094 hands over one sentence: `prepare`, then `run_phase` and `gate_phase` for
each phase of the chosen protocol, then `verify_and_publish`, then
`TaskDone` — with every state change through `state::apply`, "recorded before
its effect", and "the worktree removed and the lock released on every exit
path". Six steps that already exist, each with its own tests and its own
decisions about what it writes. Composing them is not mechanical, because four
of them have already refused ownership of something the composition needs:

- `protocol::for_task` appends [`Phase::Verify`] and [`Phase::Publish`] to every
  protocol body. Worked as ordinary phases they would start a session each: an
  agent would be asked to run the completion set, and
  `Runner::verify_and_publish` would run it a second time.
- `Runner::verify_and_publish` deliberately writes no
  [`crate::EventKind::PhaseEntered`] — its own module doc says `run_phase` owns
  entering a phase — and `Runner::run_phase` is never called for the two
  completion phases. So nothing moves the task from
  [`crate::TaskState::Running`] into [`crate::TaskState::Verifying`]. Nothing is
  *required* to: `Running` accepts `VerifyPassed` and even `VerifyFailed`, and
  the latter leaves the task in `Running` **naming the last work phase** — a
  journal that tells a recovering run the agent session is still going, after
  the session ended and its candidate was committed. VISION.md §7 chooses a
  remediation from where the task stopped, which makes the state the completion
  set was measured in worth having on the record.
- A `Publish` entry is not merely unowned: `Running` refuses it
  (`PhaseEntry::Publication` is a refusal in every state but its own), so a
  driver that journalled the ending it was told to journal — one entry per
  phase of the protocol — would stop with an `Error::InvalidTransition` in the
  middle of a published task.
- `Runner::gate_phase` decides `red` and `green` by the difference between two
  gate runs (ADR-0088), and refuses a phase it was never given a prior run for
  rather than inventing the missing half. The phase list hands it no prior run:
  the measurement that precedes a phase is not a phase.
- `git::remove_worktree` has no force, on purpose (ADR-0043): it refuses a
  checkout with anything uncommitted in it. "Remove the worktree on every exit
  path" and "the next attempt is told to read the work the attempt that stopped
  left" (VISION.md §7) cannot both be done to a dirty checkout by deleting it.

## Decision

**The run owns the ending, and writes exactly one row for it.** The loop over
`protocol::for_task(...).phases` stops at the first phase `is_ending` names
(`Verify` or `Publish`); no session is started for either. `finish_the_task`
appends the `PhaseEntered { phase: Verify }` row itself, immediately before
calling `Runner::verify_and_publish` — the row before the effect, which is what
the task's own wording asks and what makes the refusal of a completion gate
land on `Verifying { attempt }` instead of on a work phase that finished. No
`Publish` entry is written, because the machine refuses one; the ending's other
rows (`PublishStarted`, `PublishVerified`) belong to `verify_and_publish`,
which is already the only writer they have (ADR-0080, ADR-0089).

**A phase's gate runs only after its session has claimed the work is complete.**
`earned_its_gate` accepts `PhaseOutcome::Claimed` with
`ReportResult::Done` and stops the run on the two other answers — a report that
was never written, and a session that said it stopped short — *before* the gate.
Both are `Error::NotFound` carrying what they were found to be: the path the
prompt named plus the [`crate::ReportClaim`] class for the missing report, and
the agent's own header line, quoted rather than paraphrased, for the claim.
Nothing is journalled for either: `run_phase` already wrote the entry, the
output and the end, and a verdict belongs to a gate that did not run. This is
`verify_and_publish`'s own ordering — refuse an uncommitted tree before any
completion gate is spent — applied one level up, so a phase with nothing to
have proved never collects green gate rows on its way out.

**The measurement a difference is decided against is taken as the phase starts.**
`gate_baseline` runs the phase's declared gate over the checkout *before* its
session touches anything, and only for the two phases that decide by a
difference and declare a gate. The summary a phase's own gate reached is then
carried to the next phase as its starting point, so one gate run serves as one
phase's answer and its successor's baseline. It is the only point at which a
failure can still be found to be *new*.

**A clean checkout is removed; a dirty one is kept; the lock is always given
back.** `clear_ground` destructures `Prepared`, asks `git::is_clean`, calls
`git::remove_worktree` only on a *yes*, then calls `RepoLock::release` whatever
the checkout did — both questions asked, the first refusal returned. `run_task`
calls it on both ways out, and on the failure path the sweep's own fault is
discarded only so the run's refusal survives intact: a sweep that faulted
leaves the checkout standing, which is the kept checkout rather than a second
finding. `RepoLock::release` is called rather than left to `Drop` precisely
because a released lock that failed is then a reported fault instead of the
silence of a destructor (ADR-0047).

**The returned state is folded out of the journal, not remembered.** `folded`
opens the journal and folds the task's own rows through `state::apply` from
`TaskState::Queued`. Rows whose task is `NULL` are left out, for the same reason
`Journal::rebuild_state` leaves them out: they are about the queue and move no
task. A driver that returned a state it had assembled itself could report
`Done` from a journal the machine would refuse; this cannot, and an illegal row
comes back as the `Error::InvalidTransition` it is.

## Alternatives considered

- **Working `Verify` and `Publish` as ordinary phases,** since the protocol
  lists them. It starts two agent sessions whose only job is to re-run steps
  this process already owns, and it runs §8's completion set twice. The
  protocol lists them because its *word* is a statement about the whole task;
  the driver is where that statement meets the fact that an agent is not what
  pushes.
- **Letting `verify_and_publish` write the `Verify` entry.** It is the better
  home for the row by one argument — the step that is verified should be the
  step that says it began — and it is closed to this task: that module's
  documented refusal is load-bearing for `Publishing`, which takes a gates
  entry only for a later attempt, and editing a step with thirteen hundred lines of its own
  tests to satisfy a composition is scope creep in the direction that matters
  least. The driver writes it, and the refusal stays where its reason is.
- **No `Verify` entry at all.** Legal, and it leaves `VerifyFailed` journalled
  against a work phase that had already finished. The cheapest fix for a
  recovery reader is the row that makes the state true.
- **Refusing a phase whose gate has no baseline, instead of taking one.** It
  would make `tdd` runs impossible to drive at all: nothing else in the system
  runs a gate before a session, and ADR-0088 explicitly says the second run is
  what the phase is decided against.
- **Running the baseline gate after `run_phase`, from `gate_phase`.** By then
  the session has written its change, and a red phase's "new failure" is no
  longer distinguishable from a failure the checkout already had.
- **`remove_worktree --force`, or removing the checkout first and journaling
  later.** Both destroy the artifact §7's remediation is told to read, and the
  second destroys it before anyone knows whether the run needs it.
- **`TaskFailed` from the driver** for a phase that stopped. The class that row
  carries is §7's remediation's input, and choosing a remediation is the next
  task; a driver that closed a task as failed would leave the queue's successor
  unblocked while its predecessor's work sat uncommitted in a kept checkout.
- **Returning `RunOutcome`/`TaskState` plus a cleanup summary.** `Result<TaskState>`
  is the signature the task named, and the cleanup's refusal has one reader
  worth hearing — the operator, who learns it from the kept checkout and the
  journal. Threading it out is a later task's `RunOutcome`'s job.

## Consequences

- `Runner::run_task` is the only writer of a `PhaseEntered` row that a phase's
  own step did not write. Anyone adding a completion phase to a protocol adds
  it to `is_ending` or gets a session started for it; that one function is
  where the two halves of the protocol are separated, and its doc says why.
- An attempt's record is never closed here: no `AttemptRecorded` row is
  appended, so `AttemptRecord::ended` stays `None` for the attempts this driver
  opens. The journal is still legal — `Running` accepts a record at any point —
  and the task that files an attempt's evidence owns that row.
- A red phase whose task claims §9's test-first exception still gets its
  baseline gate run. The claim can only be judged against what the phase
  changed, which is unknown until its session ends, and the alternative to
  measuring is comparing against nothing. The journal then holds a gate pair
  that decided nothing, which is honest — the gate really ran.
- `run_task` is `pub`, `run_task_with` is not, and the environment accessor is
  threaded through the whole run rather than read from the process. A test that
  drives a task end to end has to aim the prompt library at a scratch
  configuration home, and `docs/DESIGN.md` Conventions keeps both out of the
  machine running the suite: no test sets an environment variable, so every API
  that reads one takes an accessor.
- Two things a real operator will want are deliberately absent: the
  `NEEDS_INPUT` pause a `PhaseOutcome` that asks for it should produce (§3's
  eighth invariant), and the circuit breaker that decides a *task* has failed
  after enough refused attempts. Both arrive at a journal this driver leaves in
  a state recovery can read, which was the point.

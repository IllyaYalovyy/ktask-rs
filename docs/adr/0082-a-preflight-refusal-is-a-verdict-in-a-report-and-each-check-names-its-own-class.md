# 0082. A preflight refusal is a verdict in a report, and each check names its own class

- **Status:** accepted
- **Date:** 2026-09-22

## Context

VISION.md:121 gives the state between `queued` and `running` exactly one job:
"*proves the world is sane before spending tokens: clean fetched mainline, green
`baseline_command`, provider available, disk space, lock acquired*." T087 is the
task that writes it, and the plan hands over one line of signature —
`preflight(project, config, provider) -> Result<PreflightReport>` — plus a
done-when with two halves: "each check fails independently in a test with the
right class" and "preflight is journaled with its evidence." Five checks and one
signature cannot settle those without five decisions, and three of them change
what a caller can do with the answer.

**A `Result` has two channels and the plan names neither for a refusal.** A check
that refused has produced an answer, and the answer is the thing the run acts on.
VISION.md §7 is read *per class* — `provider_configuration` and `needs_input`
pause for a human, `git_conflict` and `verification_failure` may be remediated —
so the class has to arrive as data the caller can match on. It cannot arrive
inside `crate::Error`: ADR-0057 records that [`crate::classify()`] derives a class
from an error of the *run*, from a `GateResult` or an `Error` arm, and it has
nothing to read a check's finding from. A preflight refusal smuggled through
`Error::Git` would be re-derived by a classifier that was never told which check
spoke.

**One check is not one kind of failure.** The mainline check asks git three
questions, and their answers are not the same kind of bad news: a remote that
will not answer and a checkout with uncommitted work in it are different
problems, and VISION.md §7 lists "branch drift, a rejected push" under
`git_conflict` while listing "the tree was dirty at verification time" under
`policy_failure`. The same split runs inside the baseline gate: a command that
ran and exited non-zero is a verdict about the code, a command that could not be
spawned at all is a verdict about the machine, and a command that ran out of its
budget is neither — which is precisely the distinction ADR-0036 made
`GateResult` carry three separate fields for, and the distinction
[`crate::classify()`] already draws (a timed-out gate is
`environment_failure`, not `verification_failure`).

**Nothing is cheap in the order the plan listed them in.** The provider is
answered in an instruction, the disk in one syscall, the mainline costs a network
round trip, the baseline gate costs up to `gate_timeout_secs` — 1800 seconds by
default (ADR-0035) — and the lock is the only answer that goes stale while the
others are being collected. A full disk discovered after a twenty-minute baseline
is a discovery that cost twenty minutes.

**Only one of the five checks has a catalog entry.** `docs/DESIGN.md` gives a
gate its `GateStarted`/`GateFinished` pair (ADR-0080) and gives the *state* its
`PreflightStarted`/`PreflightPassed`/`PreflightFailed` triple. There is no entry
that can carry "the disk had 4 GiB". And the verdict rows have an owner problem:
`PreflightFailed` is the row that ends a task (`state::apply` stops it), and
`preflight`'s signature holds no recorder — so whichever code writes
`PreflightStarted` is the code that must write the answer, or the same decision is
appended twice from two connections (ADR-0016).

**"Provider available" is unanswerable through the trait as built.**
`Provider` has `name`, `capabilities` and `invoke`. `invoke` is the only door to
"the executable is there and answered", and going through it spends a session —
against the whole point of a state that exists to precede spending — and the
`dummy` provider consumes a step of its scenario for the privilege. Adding a
probe method to the trait is a change to the adapter contract, which is a design
decision this task was not given.

## Decision

**The verdict travels in `Ok`.** `preflight` returns
`Ok(PreflightReport)` for every answer a check gave, passing or refusing, and
`Err` only for the one thing that is not an answer: preflight could not ask a
question at all. That is `Error::Database` when the project's state directory is
not there, `Error::Config` when the configuration describes a profile that cannot
be built (the mandatory-gate rule of `profile_from`), and `Error::Database` when a
gate's own row was refused. `PreflightReport` carries every check that ran, in
order, each as a `CheckOutcome` — `Passed { check, detail }` or
`Refused { check, class, detail }` — so a refusal cannot be built without naming
its class, which is where the requirement actually lives rather than in a
`bool`/`Option` pair that permits a classless refusal.

**The class comes from the cause, not from the check.** Two checks may share a
class and one check may produce two: fetch failure, an unresolved remote tip and
a git that refuses to answer are `git_conflict`; a dirty mainline tree is
`policy_failure`; a baseline command that ran and refused is
`verification_failure`, while one that could not be started or that ran out of its
budget is `environment_failure`; an adapter answering for a different CLI is
`provider_configuration`; a filesystem below `min_free_disk_bytes`, a filesystem
that cannot be asked, and a lock that cannot be taken are `environment_failure`,
which is the class [`crate::classify()`] already reads off an `Error::Io`. The
check's own name travels beside it, so an operator sees which question was asked
and the taxonomy sees which kind of answer came back.

**Cheapest first, and the first refusal ends the checks.** The run order is
provider → disk → mainline → baseline → lock, which is the order
`PreflightCheck`'s variants are declared in, and a report holds only the checks
that actually ran. A report is read in order rather than counted, so three rows
ending in a refusal says the last two questions were never asked and nothing in
the type claims an answer for them.

**The gate's rows are journaled here; the verdict's row is handed back.**
`preflight` opens the project's own journal and writes the baseline
`GateStarted`/`GateFinished` pair under `task: None`, exactly as
`run_completion_set` writes a gate that belongs to no task yet; a gate that could
not be started leaves the `GateStarted` with no answer after it, which is the pair
ADR-0080 documents as saying this gate never completed. The verdict row is
returned by `PreflightReport::event()` — `PreflightPassed { base_sha }` or
`PreflightFailed { class, detail }` with the whole report's evidence, one line per
check that ran, in the `detail` — for the caller that owns the run's recorder to
append. That is how the done-when's "journaled with its evidence" is satisfied
without a second writer ending a task twice.

**`base_sha` is empty until the mainline check has passed.** A report that
refused at or before that check names no base, because the commit is the thing
work is based on and a refused preflight has allowed no work to start; what git
said about a refusal is in that refusal's own detail, where an operator reads it.

**Provider availability is answered as an identity check, and the gap is named
rather than hidden.** The rule checked is that the adapter the run holds answers
for the provider the configuration names — the rule the *evidence* depends on,
since a run holding a `codex` adapter under a `dummy` configuration files every
later gate under a CLI that never ran the work — and the passing detail carries
the adapter's capability answer as the evidence that it is real. Whether the named
executable is installed is VISION.md:233's business: "*`ktask-rs doctor` performs
a minimal real provider preflight*", which is the task-shaped hole this decision
leaves on purpose.

## Alternatives considered

- **`Err(PreflightFailure)` for a refusal.** One channel for verdicts and one for
  accidents is the shape `Result` is for, and it is the wrong shape here: the
  class must be matched on, and `crate::classify()` cannot see it (ADR-0057). It
  also loses every earlier check's evidence, because an error carries one finding
  while a refusal needs the sequence that led to it.
- **Ask every check and report all five answers.** More readable per run, and it
  buys the same decision, at the cost of up to a gate's 1800-second budget behind
  a disk already known full. The state's purpose is to be the cheap half of a run.
- **Journal `PreflightPassed`/`PreflightFailed` inside `preflight`.** It would
  satisfy the done-when literally while breaking the invariant underneath it: the
  caller that journaled `PreflightStarted` owns the pair, `PreflightFailed` ends a
  task, and two connections appending the same decision is the duplicate
  ADR-0016's sequence numbers exist to make visible. Returning the row keeps one
  writer and costs the caller one line.
- **Stop at the first refusal *after* the baseline only, or run the lock check
  first.** Every variant was rejected for the same reason: the order that wastes
  least is a property of the costs, not of the checks, and the lock's answer
  decays fastest, so it goes last. Pinning the order in one test
  (`a_passing_preflight_records_each_check_in_the_order_it_asked_them`) means a
  later reordering is a decision someone has to make loudly.
- **Add `Provider::probe()` to the trait.** It is the right long-term answer and
  the wrong task: it changes the adapter contract every provider implements, and
  VISION.md:233 already assigns the real probe to `doctor`. A cheaper variant —
  call `invoke` with a trivial prompt — spends the token the state exists to save.
- **Check `git status` against the fetched tip and refuse on drift.** A checkout
  standing behind `origin` is ordinary in a repository that other people push to,
  and the work is based on the fetched tip rather than on `HEAD`, so drift is
  settled by publication (ADR-0046). Refusing it here would pause a queue for a
  condition that has a mechanism already.
- **`PreflightReport` as `Vec<Result<(), FailureClass>>`.** The absence of a
  passed check's evidence is the loss that matters: `PreflightFailed`'s detail is
  the only place a refusal is explained, and "disk: passed — 4123161600 bytes are
  free below …" is what makes a later reader believe the check ran.

## Consequences

- `preflight` opens a journal handle of its own. A project with no state directory
  is refused with `Error::Database` rather than having one conjured, the same rule
  `journal` and `lock` keep: registration owns that directory.
- Callers must not invent a base. `base_sha` is empty on a report that refused at
  or before the mainline check, so any code that wants a base reads
  `report.passed()` first — the field's documentation says so, and
  `an_unclean_checkout_is_refused_as_a_policy_failure_naming_the_file` pins it.
- A refused preflight writes the gate's `GateStarted` with no `GateFinished` and
  no verdict row of its own. Recovery already treats an unmatched start as "this
  gate never completed" (ADR-0080); a reader who assumed preflight is silent had
  not met a baseline whose program was uninstalled.
- `Provider available` is weaker than VISION.md:121's words. A configuration
  naming an executable that is not installed passes this check and fails at the
  first `invoke`, classed by `crate::classify()` as the provider error it is. It
  fails there rather than here; the fix belongs to `ktask-rs doctor` (VISION.md
  §4), and a task that adds `Provider::probe()` should revise this paragraph and
  the check's documentation rather than quietly strengthening it.
- The lock check takes the lock and gives it back inside the same call, so a
  preflight never holds the thing its own run later needs. It also means the
  answer is true only for the instant it was given: the run acquires its own lock
  afterwards, and the gap between the two is where another supervisor lives. That
  is inherent to a check that proves acquirability rather than holding it.
- Two files outside the plan's list changed by one line each: `mod runner;` and
  the re-export in `lib.rs`, required by `unreachable_pub` (ADR-0068's precedent).
- `PreflightCheck`, `CheckOutcome` and `PreflightReport` are exported at the crate
  root, so the TUI's state row (T104's screen) can render a refusal without
  reaching into a private module.

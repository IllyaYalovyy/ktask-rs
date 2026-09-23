# 0089. A task is committed before it is verified, and the one retry reruns every gate

- **Status:** accepted
- **Date:** 2026-09-22

## Context

VISION.md §10 is a seven-step transaction, and two of its lines are load-bearing
for how [`Runner::verify_and_publish`] is ordered: step 3 makes *a dirty tree at
verification time* a `policy_failure`, and step 4 makes *the exact candidate
commit* what final verification runs against. The step's own signature printed
those as `require_clean`, then `run_completion_set`, then `commit_all` — an
order in which the commit is written after the verdict it was verified by. Six
forces turned that into a decision.

**Read literally, the printed order refuses every task that worked.**
[`git::require_clean`] refuses on any porcelain record — staged, modified or
untracked — and a session that did its job always leaves at least one. Run first,
the check would refuse a task for the rule that exists to catch a task that did
*nothing*, and [`git::commit_all`] would then never be reached. Its mirror case
is just as sharp: a task that genuinely changed nothing has nothing staged, so
the commit that follows refuses with `nothing is staged to commit` and the run
has published nothing while its gates hold five green pairs.

**A gate run before the commit is a gate run against a different repository.**
The completion set is handed [`Prepared::base_sha`] as `KTASK_BASE_SHA` so the
privacy scan reads the range this task added (§11). Before `commit_all`, `HEAD`
*is* that base: the range is empty, and the scan that step 5 depends on passes by
having read nothing. Any gate that reads history — a diff over the range, a
`git log` assertion, a coverage gate pointed at the diff — measures the base and
is then cited as evidence about the candidate.

**Three refusals, three different kinds of fact.** A gate that ran and refused
has answered a question about the code (or, when it ran out of its budget, about
the machine: ADR-0082). A tree that holds uncommitted work is §10's own rule
broken, which §10 already names `policy_failure`. And a git call that refused —
an unreadable tree, a locked index, a hook that would not sign — says nothing
about the work at all. ADR-0057's rule is that a class arrives as data rather
than being re-derived from an error's words by [`crate::classify()`], so which
refusal writes a row is a decision, not an accident of the call order.

**Which rows may be written is decided by the state machine, not by the step's
convenience.** After [`crate::EventKind::PublishStarted`] the task stands in
`Publishing`, whose accepted rows are a gate pair, a passing verdict,
[`crate::EventKind::TaskFailed`] and a pause — and it *refuses*
[`crate::EventKind::VerifyFailed`]. The retry this step performs runs entirely
inside that state, so the second completion set can journal its pass and cannot
journal its refusal.

**A rejected push is ordinary, not exceptional.** §10 step 6 is push, fetch, and
require the fetched tip to *be* the candidate; another supervisor, or a human who
merged, makes the push a non-fast-forward. [`git::publish`] runs three commands
and reports which one refused, so a push the remote would not take, a fetch that
could not reach the remote, and a read-back that disagrees are one error type and
three different facts — only the first has a repair. [`git::rebase_onto_remote`]
applies or conflicts and, on conflict, aborts and leaves the tree as found.

**A conflict is the one refusal with a human in it.** Two sides want different
content; §7 has the class (`git_conflict`) and §16 lists "deciding by asking the
agent" as the failure to avoid. The row has to carry *which paths*, because the
reader's next command is a git command in that checkout.

## Decision

**The order is §10's: commit, require clean, verify, publish.**
[`git::commit_all`] writes the candidate under `Task {id}: {title}`;
[`git::require_clean`] then insists the tree holds nothing else;
[`run_completion_set`] runs over that commit and [`Prepared::base_sha`] stays the
privacy range's start; [`crate::EventKind::PublishStarted`] names the candidate;
[`git::publish`] pushes, fetches and compares;
[`crate::EventKind::PublishVerified`] closes it. The method returns the published
SHA. The SHA in the offer, the SHA every gate agreed to, and the SHA
[`crate::EventKind::PublishVerified`] compares with the fetched tip are one SHA,
which is what makes step 4 true rather than aspirational.

**A dirty tree, and an empty index, are policy verdicts.** Both
[`Error::Policy`] refusals append [`crate::EventKind::VerifyFailed`] carrying
[`FailureClass::PolicyFailure`] and git's own words — the paths
[`git::require_clean`] listed, or what `commit_all` left out — and no gate is
spent and nothing is offered. Uncommitted work is not a candidate, and a tree
that cannot be published must not collect green rows on the way to being refused.

**A gate refusal's class comes from the run.** A timeout is
[`FailureClass::EnvironmentFailure`] and anything that reached a verdict and
refused is [`FailureClass::VerificationFailure`] — [`verdict_class`], the same
rule ADR-0082 set for a baseline and a phase's own gate, so the three places a
gate decides cannot disagree. The row's detail carries every refusing gate's own
line, in the order they refused.

**Nothing is journalled when nothing was measured.** A gate whose program is not
there, a tree git could not read, and a git call that refused for a reason other
than divergence return their errors with no verdict row: there is no finding to
record, and a `VerifyFailed` for a broken tool blames the code for the machine.
A gate that never started keeps its lone
[`crate::EventKind::GateStarted`], which is what ADR-0036 says reads as an
incomplete gate.

**The retry reruns the whole set, and its refusal writes no row.** A push refusal
is the only error that reaches [`git::rebase_onto_remote`]. When the replay
applies, the completion set runs again from scratch against the replayed SHA —
the gates' own log in the tests is what proves this, counting names rather than
trusting a row — and only then is the replayed candidate offered, once.
[`VerdictRow`] carries the state machine's veto: `Append` for the first set,
whose verdict the attempt's state holds, and `Withhold` for the rerun, whose
refusal comes back as the returned [`Error::Gate`] with the dangling gate pair
marking where it stopped.

**One retry only.** A remote another process keeps moving is recovery's problem:
the next attempt re-preflights against a fresh base, and looping here would spend
a task's whole gate budget on a race this run cannot win. A second refusal
propagates.

**The retry commits nothing and re-checks nothing.** The candidate is already a
commit, and [`git::rebase_onto_remote`] runs `--no-autostash`, so a dirty tree is
a refusal rather than a silent move of someone's work onto no ref. `commit_all`
here would refuse because the index is empty, which is the wrong reason for the
right outcome.

**A conflict ends the task and names every path.**
[`git::RebaseOutcome::Conflict`] becomes [`crate::EventKind::TaskFailed`] with
[`FailureClass::GitConflict`] and the paths git listed, in the row and in the
returned [`Error::Git`]'s `stderr` alike; the error's argument vector is the
rebase that was attempted, and its wording deliberately avoids
[`crate::classify()`]'s phrase for a git that never started — this one started,
ran, and stopped on content. No session is invoked to choose between two people's
work.

## Alternatives considered

- **`require_clean` first, as the task text printed it.** Unimplementable for any
  task with work in it (above), and it leaves step 4 false: no commit exists to
  be verified against. VISION.md is authoritative, so the step follows §10 and
  this ADR records the difference.
- **Verify the tree, then commit exactly what was verified.** Saves one
  `require_clean` call and breaks the privacy range: `HEAD` still equals
  `KTASK_BASE_SHA` while the scan runs, so §11's gate reads an empty diff. The
  commit would also be made *after* the answer that justified it, which is the
  shape §3's third invariant exists to prevent.
- **Journal `VerifyFailed` on the rerun anyway.** One legal-looking row, and the
  loss is the journal: [`crate::Journal::rebuild_state`] cannot project past a
  row `Publishing` refuses, so the run would destroy the recoverability that is
  the whole point of journalling it.
- **Journal `TaskFailed{VerificationFailure}` on the rerun.** Legal, and it ends a
  task whose first candidate passed every gate: the only new fact is that the
  replay landed on somebody else's commit. The next attempt re-bases onto a base
  that already contains that commit and can pass, which is what
  `Publishing`-plus-error preserves.
- **`Paused{reason}` or `DecisionRaised` for a conflict.** `Publishing` accepts the
  first and refuses the second, and neither carries a path: `PauseReason` has five
  variants and all five are argument-free or carry an instant. A stopped task whose
  message does not say which file to open sends a human to read git themselves.
- **Loop until the push is accepted, or push with `--force-with-lease`.** The loop
  spends a task's gate budget on a race; force-with-lease discards a peer's work,
  which §10's step 6 comparison rules out.
- **Reuse the first verdict for the replayed commit.** §10 step 4's "exact
  candidate" is a different commit after a rebase, and reusing evidence gathered
  against a SHA that no longer exists on the mainline is the pattern §16 ranks
  first.
- **`commit_all` and `require_clean` again before the retry.** Both would refuse
  for the empty index, and a check the code has to arrange to be vacuous is a
  check nobody can read.
- **Let [`git::publish`] do the rebase.** It would have to journal rows to be
  useful, and `git.rs` deliberately holds no journal and no policy.
- **Have this step journal its own [`crate::EventKind::PhaseEntered`].**
  [`Runner::run_phase`] owns entering a phase; two writers of the same row give
  recovery two candidates to reconcile, and `Publishing` takes a gates entry only
  for a later attempt.

## Consequences

- No path reaches publication without a passing completion set, and the shape is
  checkable from the journal alone: every `PublishStarted` sits behind a
  [`crate::EventKind::VerifyPassed`] for the same attempt, and
  `published_verified` only where the fetched tip equalled the candidate.
- A clean divergence now recovers without a human: two complete gate runs, two
  verdicts, two offers, one proof. A conflicting one stops with a class and a
  path list rather than with an agent asked to pick a side.
- `verify_and_publish` has no caller yet, and its entry state is a driver's
  responsibility: it needs a task in `Verifying` or `Running`, because
  `Preflight` refuses a verdict row. T094's walk owns entering `Phase::Verify`
  before calling it; the tests journal that row by hand for exactly that reason.
- [`git::commit_all`] stages tracked files only (`git add --update`), so a task
  that adds a brand-new file and never stages it reads as §10's policy failure
  naming that path. Kept deliberately: staging a file the agent created is a
  decision, and the alternative is silently publishing it. The rule is worth a
  phase-level convention before it bites a real run.
- After a replay the privacy range starts at the same `Prepared::base_sha` and now
  ends at a commit that also contains the peer's work, so the rerun's scan covers
  more than the first scan did. That is the stricter direction, and the direction
  §11 wants; a scan that reported the peer's own secret would be a finding about
  the mainline, not about this task.
- `gate_timeout_secs` still applies per gate, so a rerun doubles the budget a task
  spends on gates. Bounding the retry bounds the total at two sets, which is the
  most this step can promise.

# 0092. A refusal is answered by one fresh session, in the checkout the refusal left

- **Status:** accepted
- **Date:** 2026-09-23

## Context

VISION.md §7 is eight bullets and one sentence about what a repair may never
touch, and every piece it needs already existed with no caller: [`classify`]
sorts a failure, [`bundle`] assembles what a fresh session is told,
[`Bounds`]/[`should_continue`] and [`Breaker`]/[`signature`] decide whether
there is a next session at all, [`read_evidence`] reads the attempts before this
one off disk, [`check_no_policy_edit`] knows which paths an attempt may not
edit, and ADR-0091 gave a recovery its account. What did not exist was the loop
that asks them in order, in the middle of the step that drives a task.

Five questions had to be settled where §7 states a requirement rather than a
mechanism, and a sixth turned out to be a defect in the obvious way of writing
it.

**Which of `Config`'s two attempt keys bounds the loop.** `max_attempts` is
`docs/DESIGN.md`'s "one normal, one remediation" and defaults to 2;
`max_remediation_attempts` defaults to 1 and had no reader anywhere.
[`Bounds::max_attempts`] describes itself as "the ceiling on how many of those
launches one task's failure may still pay for", which is the second key, not the
first.

**What bounds elapsed time.** §7 names wall-clock time as a bound and `Config`
has no key that names it.

**What to do about tokens.** §7 names a token budget; `Config` holds no ceiling
for one, and every [`crate::Usage`] field is an `Option` because providers
report nothing.

**Which classes never loop.** §7 names two: `provider_configuration` and
`needs_input`. `git_conflict` is not one of them, and it is the one class whose
step ends the task before any loop could see it.

**Where the measurements a refusal is classified from live.** [`classify`] reads
an [`crate::Outcome`] and a slice of [`crate::GateResult`]. The journal holds
rows *about* them, and nothing on disk holds them in the shape they are asked
in.

**The defect.** The first version reached the ending through the door a caller
outside a run uses. A completion set that refused therefore arrived at the loop
with no gate evidence at all, and [`classify`]'s fallback blamed the agent for a
gate's verdict.

## Decision

**The loop lives inside the step that drives a task**, between the preflight's
[`Prepared`] and the step that gives the checkout and lock back. A retry loop
above that step would have to re-prepare to start a second attempt: a second
fetch, a second checkout, and — decisively — a second base SHA, so the repair's
diff and the refused attempt's diff would be measured against two different
commits. §7's "preserve the worktree and all prior attempt evidence" is a
requirement about identity, not about disk space.

**One struct makes §7's contradictory pair visible instead of merely intended.**
§7 says carry the refused attempt's worktree and evidence forward, and carry not
one of its measurements forward. `UnderAttempt` holds the ground and the repair
— which come across the loop — beside the [`Witness`] of what this attempt was
*seen* to do and the flag saying whether it has accounted for itself, and those
two are built afresh every iteration. That is how "no cached gate result
survives into the second attempt" is true by construction: a value that was
never constructed cannot be reused, and there is no cache-clearing call for a
later edit to forget.

**The attempts bound comes from `max_remediation_attempts`, not `max_attempts`.**
[`should_continue`] stops when the refusals exceed the ceiling and says so in its
own documentation: "a ceiling of zero stops at the first refusal, which is how a
project that allows no retries at all says so". Only the repairs key satisfies
that reading. Feeding it the total would let a project with `max_attempts = 2`
and `max_remediation_attempts = 1` have two repairs while its own settings said
one. `max_attempts` is not dropped — it is the time bound's denominator, below.

**Elapsed time is derived: `attempt_timeout_secs × max_attempts`.** One session
is bounded by the first key and one task's sessions by the second, so their
product is the window this configuration has already said it will spend on a
task; past that the run is waiting for something its own settings do not
contemplate, which is the only thing a derived bound has earned the right to
stop. It is measured from an [`std::time::Instant`] taken before the first
attempt rather than summed from the sessions' own durations, because a session
that hung for an hour cost an hour and the bound exists to stop a run waiting
for it.

**Tokens are counted and unbounded, and said out loud.**
[`Bounds::max_tokens`] is `None`: `Config` holds no ceiling, and a bound that
cannot be measured must stop nothing. The spend is still gathered from every
session's [`crate::Usage`], so the day the configuration grows a ceiling the
figure it compares against is already being collected.

**Never-looped is §7's two, plus `git_conflict` for a different reason.** A
`provider_configuration` or `needs_input` refusal returns to the caller with
**no row added**: a pause is not a failure, §3's eighth invariant's pause is a
later task's, and a `TaskFailed` invented here would be read by every screen as
a task that was refused rather than one that asked. A recovery that stopped this
way still files its account, because it did try a session. `git_conflict` is
handed back too and files nothing, because [`Runner::stop_on_conflict`] has
already journalled the `TaskFailed` that ends the task, and no state holds a row
after the ending.

**A protected path is looked at before a single bound is spent**, on a first
attempt as on a repair. An attempt that edited `clippy.toml` or `scripts/`
rewrote the examination it is about to pass (§3's fifth invariant), so
[`check_no_policy_edit`] is asked of every attempt's changed paths *before*
[`crate::protocol::check_scope`] and before the phase's gate can be run against
the edited rule; the paths travel out with the refusal so the row that ends the
task names the files a human must look at. Spending the bounds first would
journal a sentence about a counter over a refusal whose cause is a forbidden
path.

**The account comes before the ending, and only a repair files one.** ADR-0091
left this ordering as a consequence for this task to discharge:
`SelfHealingReport` is admitted by `Remediating` and by no other state, and the
ending's own `PhaseEntered` leaves `Remediating`. `file_the_account` is a no-op
for an attempt that is not a repair, and its `accounted` flag makes a second
call a no-op — the account is filed before the ending, the ending can still
refuse, and a refusal arriving on a path that already filed one must not become
a second failure.

**The bundle is read from disk before the next attempt is opened.**
[`read_evidence`] is asked at the last step of answering a refusal rather than
the first, because the next `begin_attempt` files a record of its own: the
bundle has to say how many attempts *there were*, and one that counted the
repair it is about to launch is off by one in the first line a session reads.

**The bundle is appended under its own heading, and no session id exists to
pass.** [`crate::context::build_prompt`] is left alone (ADR-0075): a repair is
handed the same documents its predecessor was, and what it gets *extra* is the
supervisor's account of why it is being asked again, under a heading that says so.
`Remediation` holds a class and a bundle, and [`crate::Invocation`] has no field
that could carry a session id, so §7's "session resume is never relied on" is
mechanical rather than remembered. The test for it stamps an id on every session
the adapter answers, because reading back the absence of a thing that was never
there would prove nothing.

**The ending goes through the attempt's own witness.** `finish_the_task` calls
`publish_the_attempt`, not the single-phase [`Runner::verify_and_publish`], for
the same reason the phases call their internal halves: the completion set's
[`crate::GateResult`]s are the evidence a refusal of the ending is classified
from, and the public door builds a witness and drops it. Passing gates are kept
as well as refusals, because the account says which gates ran again and a
signature that counted a gate which passed would stop two identical refusals
from looking identical.

## Alternatives considered

- **A retry loop above the step that drives a task.** One fewer struct, and the
  loss is the point of §7's second bullet: re-preparing means a new base SHA, so
  the repair's diff is measured against a commit the refused attempt never had.
- **Re-classify from the journal at refusal time.** The journal does hold
  `AgentOutput`, `AttemptFinished` and `GateFinished`. It lost because a
  [`crate::Outcome`] is what a provider answered and a [`crate::GateResult`] is
  what a command came back with; rebuilding either from its own summary row is
  reading the run's account of itself and calling it evidence.
- **Give the second attempt the first one's `Witness` and skip the gates that
  already passed.** Faster, and §7's last-but-one bullet forbids it in as many
  words: "no cached evidence survives a file change".
- **Bound the loop by `max_attempts`.** It is the key that reads like the
  answer, and it is the total. It lost because the ceiling a remediation is
  bounded by is the number of remediations.
- **Sum the sessions' durations for the elapsed bound.** No clock reads, and the
  loss is a hung session, which is the case the time bound exists for.
- **Invent a token ceiling** — some multiple of the bundle size or of
  `max_attempts`. It lost for the reason [`Bounds::max_tokens`] is an `Option`:
  a bound nobody configured would stop runs for a figure nobody asked for, and
  `None` is the spelling `remediate` already provides for exactly that.
- **Let `git_conflict` loop like the other loopable classes.** One fewer special
  case, and the journal would hold the ending twice — the second row landing
  where no state admits it.
- **Spend the bound before checking the changed paths.** One fewer branch, and
  the row that ends the task would say `attempts 1 past the 1 bound` about a
  refusal whose cause is a protected file.
- **Hand the previous session id to the repair "for context".** It is the token
  economy §7 explicitly ranks below determinism and reproducibility, and a field
  that exists gets used.
- **End a never-looped refusal with a `TaskFailed`.** The run would return a
  failure the way an ordinary refused attempt does, and the pause §3's eighth
  invariant asks for would be indistinguishable from it in the only place either
  is read.

## Consequences

- Every phase-shaped step now works from a `&mut UnderAttempt`:
  `run_the_session`, `gate_the_phase`, `verify_completion`,
  `republish_after_rebase`, `publish_the_attempt`, `finish_the_task`. The
  single-phase public doors (`run_phase`, `gate_phase`, `verify_and_publish`)
  build `UnderAttempt::alone` with no repair, so they behave exactly as they did
  before a run could be retried — a repair is a fact about a run, not about a
  step, and the doors cannot ask for one.
- `max_remediation_attempts` is now the only key that bounds a repair, and
  `max_attempts` bounds no attempt count anywhere: a project that sets
  `max_attempts = 9` and leaves remediation at 1 gets one repair. If §6 ever
  needs `max_attempts` enforced as a total, that is its own decision with its own
  row; the elapsed bound is where that key is used now.
- A refusal that gets no repair now ends the task with a `TaskFailed` row where
  it used to leave the task parked mid-run. Tests that examine one refusal's
  ending set `max_remediation_attempts = 0` — the configuration's own way to
  look at one refusal in isolation — and the repair's own module keeps the
  scenario shaped for a second session.
- The breaker is one per run, not one per project lifetime: a threshold of 2
  stops the second identical refusal within a run, and two identical refusals in
  two separate runs count one apiece. §7 does not say which it wants; a
  breaker that survives runs is a separate decision with a store behind it.
- The elapsed bound is a reading of intent rather than a configured figure, and
  it is the one part of this ADR to revisit if `Config` grows a key for
  remediation time. Its derivation is written next to the code that does it
  because nothing in the configuration names it.
- A repair reruns the gates of the phases it works and the whole completion set;
  nothing reruns a *baseline* for its own sake, because a baseline is a
  measurement of the tree before the task and §7's "every completion gate reruns
  from scratch" names completion gates. A `tdd` repair works red again and is
  measured again, which is the same result by the route §9 requires.
- A run that refuses *itself* — evidence it could not read, a tree it could not
  diff — is not classified and not repaired: the bundle step's own failures come
  back as they came. A supervisor that retried its own inability to read the
  journal would be spending the budget it is out of.

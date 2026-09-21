# 0072. A red phase is confirmed by the name that newly fails, not by the count that moved

- **Status:** accepted
- **Date:** 2026-09-21

## Context

ADR-0068 declared the `tdd` protocol and left a reservation beside it: a phase
declares its check, and running that check is the runner's. ADR-0071 enforced
what a red phase may *write*. VISION.md §9 step 2 is the remaining half — "The
runner executes `targeted_test_command` and confirms the expected *new*
failure" — and T077 is that confirmation. Three forces make it a decision
rather than a set difference.

The claim is about time, not about state. §9 opens with "No final test run can
prove tests were written first", and one run cannot answer a question about when
a failure appeared: `1 failed` is the same report whether the test was written an
hour ago or in this phase. Only a comparison of the phase's run against the run
that preceded the agent's work separates a test written to fail from a suite that
was already red — which is why the function takes two `TestSummary`s rather than
one.

The evidence has to name something. `Phase::Green` re-runs the tests that just
failed to prove they pass now, and §9 files RED and GREEN evidence with the
attempt (ADR-0065), so a verdict with no names would settle the phase and leave
the next one without an object. `gate.rs` already chose names for the same
reason: `TestSummary::failures` is documented as holding them because "a name is
what `GateKind::Targeted` re-runs".

The third force is that `TestSummary` holds counts *and* names, deliberately with
no verdict between them (ADR-0039), and the two disagree about newness in both
directions. Repairing an old failure lowers the count while adding no failure
that was not there before; renaming a failing test holds the count steady and
produces a name nobody has read. So which of the two answers decides what a red
phase is allowed to pass on.

## Decision

1. **`verify_red(before, after) -> Result<Vec<String>>` takes the difference of
   the two name lists.** A name `after` lists and `before` does not is a newly
   failing test; the function returns every one of them, in the order `after`
   listed them, so re-reading the same retained output compares equal.

2. **The counts are not read at all.** A count moving is not a test being new,
   and the lists are not a suggestive sample of the run: `parse_cargo` refuses
   output whose `failures:` block holds a different number of names than that
   output's own result line counted (ADR-0039), so a summary that exists at all
   names every test that failed. A difference over a complete list is a complete
   answer.

3. **A name two test binaries both reported is returned once**, at the position
   the run first wrote it. One `cargo test` is several binaries and `parse_cargo`
   sums them, so the same test name can legitimately be listed twice; green
   re-runs a test by name, so evidence naming it twice would read as two
   obligations.

4. **An empty difference is refused as `Error::Gate` under `GateKind::Targeted`**
   — the gate `tdd`'s red phase declares, so the refusal arrives attributed to
   the check that produced it. `classify` names no class for a gate error
   (ADR-0059), which puts an attempt refused here in `FailureClass::AgentFailure`
   unless the caller's gate list argues otherwise. That is the wanted landing: a
   bounded fresh session answers this refusal by writing a test that really fails.

5. **The refusal quotes both runs.** The sentence states the rule and then
   `failing before: …; failing after: …`, each list quoted name by name or spelled
   `nothing`. The inspector and the failure bundle then show the comparison an
   operator can check against the retained gate logs, rather than a phase that
   merely "failed".

6. **Nothing calls it yet**, as with ADR-0066 through ADR-0071: the runner that
   walks a protocol's phases is the task that wires `check_scope` and `verify_red`
   together and files what this returns beside the gate log ADR-0065 already gives
   the attempt. `Phase::Red` has declared `records_evidence` since ADR-0068, so
   the storage door is not waiting on this.

## Alternatives considered

- **Compare the `failed` counts** — accept when `after.failed` is larger. Wrong in
  both directions, and each direction is a test: a phase that repairs one old
  failure and breaks nothing *raises* the difference between the runs while adding
  no new failure at all, and a phase that renames a failing test adds a name while
  the failure stays as old as the baseline. A count is what a run summed, not what
  a phase caused.
- **Return the whole `after` list.** One line, and it is what a red phase looks
  like from outside. It lost because the failures it hands to green may be as old
  as the baseline, and then green re-runs tests nobody claimed and the evidence no
  longer says what was written first — the one thing §9 asked it to say.
- **Refuse with `Error::Policy`.** The rule is protocol-shaped, so the variant is
  tempting, and it is the wrong punishment: a policy failure earns no retry
  (ADR-0071 spells out why that is right for a scope breach), and here the whole
  point is that the agent can and must try again. It also carries `paths`, and
  there is no path to name — the offender is a test that did not fail.
- **Answer `Ok(vec![])` and let the caller test it.** §9 makes the empty case the
  refusal case, not a value, and a caller that forgot to ask `is_empty()` would
  file a red phase that produced nothing as evidence that it produced something.
- **Refuse a `before` that was already red.** `GateKind::Baseline` owns the
  question of whether the project was green before the task, and this one asks
  only what the phase changed. Folding the two together would refuse a task that
  legitimately begins work in a suite mid-repair, and would make the answer
  depend on a fact this function was not given.

## Consequences

- A red phase that ends with the failures it started with, or with none, ends the
  attempt on a refusal quoting both lists, and its agent gets the bounded fresh
  session `classify` grants an agent failure.
- `verify_red` is the second half of §9's red enforcement and the first half that
  cannot be satisfied by editing files. ADR-0071 stops an agent writing production
  code in red; this stops it claiming red without a failing test. Neither subsumes
  the other, and a runner that calls only one runs a `tdd` protocol that is
  `direct` with extra steps.
- The green half — §9 step 4, "the runner confirms the new test passes" — is not
  here. This task named `verify_red`; confirming that the names returned here pass
  again is its mirror but not its decision, because a name that vanished between
  the two runs rather than passing is a third answer that needs its own ruling.
- Counting is now load-bearing in one place only: `parse_cargo`. A test output
  format whose failure list is incomplete is refused there, so it cannot reach here
  and be read as "nothing new failed". That is the dependency this choice takes on,
  and it is the one ADR-0039 already accepted.

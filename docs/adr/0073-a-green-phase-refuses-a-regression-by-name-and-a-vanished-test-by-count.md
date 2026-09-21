# 0073. A green phase refuses a regression by name and a vanished test by count

- **Status:** accepted
- **Date:** 2026-09-21

## Context

ADR-0072 ruled the first half of VISION.md §9's `tdd` enforcement and named the
second half as an open question rather than a mirror image: "a name that vanished
between the two runs rather than passing is a third answer that needs its own
ruling". §9 step 4 is one line — "The runner confirms the new test passes" — and
three forces make answering it a decision.

Green holds the whole tree open. ADR-0068 gave `Phase::Green`
`WriteScope::All`, because an implementation cannot be written inside test paths,
and ADR-0071 gave that scope to nothing else. So the phase can break any test in
the repository while its own test goes green, and from outside the phase an edit
that broke an unrelated test and an edit that fixed one report the same thing:
`test result: ok. 12 passed; 0 failed` — from the run that mattered.

The type cannot say that a test passed. `TestSummary` (ADR-0039) holds three
counts and the names that failed, deliberately with no verdict between them, and
it has no list of names that passed. A name is therefore confirmed to pass by not
appearing in the failure list, and that is either entirely sound or entirely
hopeless depending on whether the run ran the test at all.

The verdict belongs to one run, not two. Red's evidence was a difference between
two runs; green's baseline is not another measurement but a fact the phase
already established — `verify_red` handed over every name that newly failed, and
green re-runs the same `targeted_test_command`. Which of the two answers a second
`TestSummary` would add is the question step 5 ("cleanup allowed while targeted
tests stay green") asks again, so the answer had to be reusable there.

## Decision

1. **`verify_green(expected, after) -> Result<()>`.** `expected` is what
   `verify_red` returned for the phase; `after` is one run of the same targeted
   command. The function takes no `before`, because the phase's own red run is
   its baseline and `expected` is what that run filed as evidence.

2. **Two refusals, one sentence.** A name `expected` holds that `after.failures`
   still lists is the test the phase existed for, still failing. A name the run
   lists that `expected` does not was not failing when the phase started and is
   failing now: a regression. Both buckets are quoted in the one refusal, name by
   name or spelled `nothing`, so the phase is never refused for half of what the
   run reported.

3. **A regression is refused by name, and that is the point of the rule.** §9
   step 4 read as "the new test passes" is satisfiable by an implementation that
   passes its test and breaks four others, and the four would otherwise surface
   at §9 step 6's verification gate, after the attempt had been called good and
   with none of the phase's context attached.

4. **Absence from the failure list is read as a pass, and the run's passing count
   is read as a floor.** The absence is sound because of what `parse_cargo`
   refuses (ADR-0039): a `failures:` block holding fewer names than the run's own
   result line counted, and a transcript that opened a test binary and never wrote
   its count. A summary that exists at all therefore names everything that did not
   pass. What it cannot see is a test that did not run — renamed out of the filter,
   deleted, or given an `#[ignore]` — so `k` distinct names are confirmed only by a
   run reporting at least `k` passing tests. Names are counted once, as in red, so
   a name written twice is one test to prove.

5. **An empty `expected` is not refused here.** `verify_red` cannot return an empty
   list, so emptiness is red's refusal and green inventing the same rule would make
   its contract depend on a caller-side invariant that is already enforced one step
   earlier.

6. **Both refusals are `Error::Gate` under `GateKind::Targeted`** — the gate
   `tdd`'s green phase declares. `classify` names no class for a gate error
   (ADR-0059), so an attempt refused here lands as `FailureClass::AgentFailure` and
   earns the bounded fresh session that can repair a regression. A `Policy` failure
   would end the task instead, and a broken unrelated test is precisely what a
   fresh session is for.

7. **Nothing calls it yet**, as with `check_scope` and `verify_red`: the runner that
   walks a protocol's phases wires the three together, and ADR-0065's evidence home
   is where §9's GREEN record (command, output, tree hash) goes — the phase has
   declared `records_evidence` since ADR-0068.

## Alternatives considered

- **Take two summaries and diff them, like red.** Symmetric and one less idea, and
  wrong twice over: the `before` it would diff against is red's `after`, which the
  phase already journalled, and a second copy of a fact is a second chance to
  disagree with the first. `expected` *is* the baseline, in the form red was
  required to file.
- **Accept when `after.failed == 0`.** Simplest possible rule, and it fails the
  done-when: it cannot name the test it refused for, so an operator reading the
  attempt sees "the targeted gate failed" and no way to know whether the new test
  never went green or the implementation broke six others. It also accepts the empty
  run, which is the dodge below.
- **Require the named tests to appear in a list of passing names.** The honest
  version of rule 4, and impossible in this type: `TestSummary` has no such list,
  and libtest writes passing names to a different stream than the summary this
  parser is built from. The floor costs one comparison and refuses the case that
  matters; widening ADR-0039 for the rest is a trade to make if a real run ever
  needs it.
- **Read the count as a population check** — refuse unless `passed + failed`
  matches the red run's total. Rejected because a green phase legitimately *adds*
  tests, so the population is expected to grow, and because it needs the `before`
  summary rule 1 deliberately does not take.
- **Tolerate a failure nobody named, on the theory it predates the phase.** That is
  §3's invariant 4 in the wrong direction: taking the run's account of its own
  innocence from the run. If a suite really was red when the task started,
  `GateKind::Baseline` is the door that owns that state, and an old failure
  reaching green is refused here by name — the safe direction, since the refusal
  quotes the name an operator can rule on.

## Consequences

- A green phase that confirms its test and breaks another ends the attempt with a
  sentence naming both, and its agent gets the fresh session `classify` grants an
  agent failure. The regression cannot reach the verify gate disguised as a
  finished phase.
- The floor is a floor, not a proof. A test swallowed by a run whose population is
  large enough still reads as green here. Two things hold the rest of that door
  shut outside this predicate: the re-run has to be the *same* command red ran, and
  §9 step 6's full verification runs the whole suite after the phase that could
  have shrunk it. If a future run summary carries the names that passed, rule 4
  should become a name check and this paragraph should be revisited.
- §9 step 5 — "refactor: cleanup allowed while targeted tests stay green" — is the
  same predicate over the same names, so the runner calls this function again there
  rather than inventing a weaker one. No separate refactor rule is added.
- `check_scope`, `verify_red` and `verify_green` are now the whole of what §9's
  `tdd` protocol can enforce mechanically: what a phase may write, that red was
  really red, that green did not break anything. None of the three is a prompt
  instruction, and none of them runs a command; the runner that walks a protocol's
  phases is the task that wires all three and files what they return.

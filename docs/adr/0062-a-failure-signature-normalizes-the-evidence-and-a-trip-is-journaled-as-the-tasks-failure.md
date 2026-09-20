# 0062. A failure signature normalizes the evidence, and a trip is journaled as the task's failure

- **Status:** accepted
- **Date:** 2026-09-20

## Context

VISION.md §7 bounds self-healing with two rules: repeated *identical* failure
signatures are detected, and a circuit breaker then trips. T067 fixes the two
doors — `signature(class, gates) -> String` and `Breaker::record(sig) ->
BreakerState` — and leaves four decisions that neither the task body nor
`docs/DESIGN.md` makes for them.

- **What a failure is recorded in.** The evidence a failure arrives as is a
  `GateResult`, and almost none of it survives being seen twice: `duration_ms`
  is the runner's stopwatch, the transcript carries the checkout the command ran
  in and the line a panic landed on, and libtest prints how long the run took
  and lists its failures in the order they finished. Two runs of one broken test
  are the same failure in every way except these.
- **What counts as "the same" twice.** A counter is easy; the answer it gives is
  the hard part. Count everything and a rerun that took longer never trips;
  count too little and two unrelated failures spend one budget, which stops a
  run that was making progress.
- **Where a trip goes.** VISION.md §3's invariant 3 puts every transition in the
  journal before its side effect, so a trip that lives only in memory is a
  decision a recovery cannot see. But `docs/DESIGN.md` admits no catalog entry
  without the `state::apply` arm that answers it, and this task owns neither.
- **What the threshold counts.** `circuit_breaker_threshold` defaults to three
  and `docs/DESIGN.md` glosses it as "identical signatures before tripping",
  which says what is counted and not over what window.

## Decision

**A signature is the failure class hashed with the sorted, deduplicated,
normalized names of the tests that refused** — SHA-256 over the class name and
one name per line, written as the first 16 lowercase hexadecimal characters. The
class is inside the hash rather than beside it because the class is what decides
the response (VISION.md §7): one test refusing once as a verification failure
and once as an environment failure is two failures with two correct responses,
and counting them together would spend a breaker on a machine that changed
underneath the run. The class contributes the spelling the journal already
writes it with, so no second vocabulary is created here.

**Names come from `parse_cargo`** — the same reading the failures screen lists
its failures from, so the signature and the screen cannot disagree about which
tests refused. Names are taken from the set, not the sequence or the counts:
order is libtest's scheduling, and one test that two test binaries both reported
has not become two failures.

**Normalization removes exactly what a rerun changes.** Four ordered rules, each
one regular expression compiled once behind a `OnceLock` the way every phrase
table in this crate does (ADR-0059), each with a test that removes only its
rule and fails: a duration (a number beside a unit — the unit goes with the
number, or `took 3s` and `took 4200ms` leave `took s` and `took ms` behind and
become two refusals); every directory segment, keeping the file it names, so one
file in two checkouts is one failure; every run of digits, which is where line
numbers, counters and parametrised case numbers all live; and finally every run
of whitespace, colons, dots and slashes, which is what the stripping leaves.
Underscores survive because they join the words of a test name.

**A gate that refused without naming a test is named by its kind beside the first
line it wrote** (stderr first, because that is where a build or a lint puts its
verdict). Without this every nameless refusal — a build that never compiled, a
lint that never ran a test — collapses into one signature, and three unrelated
build failures would trip a breaker on work that was being fixed between them.
A gate that refused in silence is its kind alone, which is still stable across
two runs of it.

**Digit stripping merges names that differ only by a digit, and that is the
accepted cost.** `case_1` and `case_12` are one body of code with two inputs, and
the response VISION.md §7 wants for both is the same. The rule to revisit if
that ever counts two different failures as one is `DIGITS`, and
`two_instances_of_one_parametrised_test_are_one_failure` is the test that moves
with it.

**The breaker counts a signature, cumulatively, for its own lifetime.** One
entry per signature, so a failure that alternates with a different one is still
the same failure returning: an implementation that remembered only the last
signature would let `A, B, A, B, …` run forever, which is the exact loop the
breaker exists to end. The caller owns the lifetime — a breaker belongs to one
task's remediation, and the runner constructs it from
`Config::circuit_breaker_threshold`.

**A trip is sticky, and a trip carries the signature that caused it.** Once a
signature has reached the threshold, `record` answers with that trip whatever
arrives afterwards: a run that tripped and then failed for a slightly different
reason has not been fixed, and re-arming on a differently worded transcript is
how a bounded remediation becomes an unbounded one. The signature rides along in
`BreakerState::Tripped` because after a sticky trip it is not the signature the
caller is holding.

**The trip is journaled as `TaskFailed`.** `trip_event(class, signature, seen)`
builds the catalog entry `docs/DESIGN.md` already gives — the class the failure
was classified as, and a detail naming the signature and how many times it
arrived — and the machine already fails a task on a `TaskFailed` from `Running`
and from `Remediating` (ADR-0022), so the projection lands in `Failed` with no
new arm and no schema change.

## Alternatives considered

- **Hash the gate's whole transcript.** It is the input with the most
  information in it and the least stability: the stopwatch, the checkout path,
  the panic line and libtest's duration are all in there, so the signature would
  change on every rerun and the breaker would never trip at all.
- **Hash the counts (`failed = 1`) instead of the names.** Cheaper and shorter,
  and the worst answer available: two unrelated failures with the same count look
  identical, so the breaker stops a task that was being repaired between them.
- **Count consecutive repeats only.** It reads as the plainer reading of
  "repeated", and it never trips on an alternating pair of failures — which is
  precisely the case where a run is spending sessions without progress.
- **Add a `CircuitBreakerTripped` catalog entry.** It names the event more
  precisely, and `docs/DESIGN.md` refuses it: an entry may only arrive with the
  `state::apply` arm that answers it, and the arm for a trip *is* the arm that
  already fails a task. `TaskFailed` loses nothing an operator needs — the class
  is its own field and the detail names the signature.
- **Carry the full 64-character digest, or a longer prefix.** A signature is
  printed on the failures screen and quoted in a report as well as compared; 64
  bits is far past what comparing the handful of signatures one task produces
  requires, and `paths.rs` truncates the project id for the same reason.
- **Return a `Signature` newtype.** Better typing at the call site, and it is
  available to the runner task that consumes this one; T067 fixes `-> String`,
  and a `BTreeMap<String, u32>` keyed on the content is what the breaker needs
  whatever the wrapper is called.

## Consequences

- `remediate.rs` now answers "have we seen this before" for the runner, the
  failures screen and `--json` alike, from one function, with no clock, no
  randomness and no memory of a previous call.
- `Config::circuit_breaker_threshold` finally has a meaning, but no reader yet:
  wiring `Breaker::new(config.circuit_breaker_threshold)` into the loop, and
  calling `trip_event` where a trip is recorded, belong to the runner task. So
  does the failure bundle VISION.md §7 describes, which needs a diff summary and
  a prior attempt's evidence that only the runner holds.
- Normalization rules are a maintenance surface. Each of the four is one
  expression with a test that names what it removes; a new test-runner whose
  output names failures differently is a new rule here, not a new caller.
- Whether the tests would notice a broken rule is measurable rather than
  argued: each of the four rules removed is caught by the one test that varies
  only what that rule strips.

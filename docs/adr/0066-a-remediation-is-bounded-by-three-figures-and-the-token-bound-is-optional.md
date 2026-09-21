# 0066. A remediation is bounded by three figures, and the token bound is optional

- **Status:** accepted
- **Date:** 2026-09-20

## Context

VISION.md §7 lists the bound among the hard limits every recovery stays inside:
"Bound remediation by attempts, elapsed time, and token budget." ADR-0062 gave
§7 the failure-shaped half of that sentence — a `Breaker` counts identical
failure signatures and trips `circuit_breaker_threshold` — and it counts none of
the three figures this sentence names. The gap is not theoretical. A remediation
that fails *differently* every attempt produces a fresh signature each time, so
no threshold is reached, the breaker never trips, and the loop goes on spending
attempts, wall clock and tokens. A breaker is the right object for a failure that
repeats; it is not a budget.

T071 fixes the shape: `Bounds` holding the three ceilings and
`should_continue(bounds, attempts, elapsed, tokens) -> Decision` answering
Continue or Stop with the reason. Four decisions are left to this module. Which
`Duration` measures the time bound; how a ceiling that cannot be measured is
represented; whether a ceiling exactly met counts as spent; and what the answer
says when more than one ceiling has been spent.

## Decision

The bound lives beside the breaker it complements, and `Bounds`, `Bound` and
`should_continue` are re-exported from the crate root as every other module's API
is.

1. **`time::Duration`, not `std::time::Duration`.** The crate already splits its
   durations by what they are for: the modules that decide carry `time::Duration`
   (`attempt`, `classify`, `state`, `event`, and the wait plan of ADR-0061), while
   `std::time` belongs to the modules that watch something run — a gate polling a
   child process, a provider adapter timing a session. A bound is a figure a
   decision is made from, so it is data, and `should_continue` stays free of a
   clock. `Bound::Elapsed` then carries whole seconds as an `i64` from
   `Duration::whole_seconds`, because a `time::Duration` is signed and a bound
   read as a count of seconds has to be able to say so rather than refuse the
   figure it was handed.
2. **`max_tokens` is an `Option<u64>`.** A ceiling that cannot be measured must
   not stop work, and a token figure is a provider's answer rather than a fact
   about the work: `Usage::input_tokens` and `Usage::output_tokens` are
   `Option` precisely because some adapters report nothing. `None` means no token
   ceiling exists. It is not `Some(0)`, and the `tokens` figure stays `0` for as
   long as nothing has been reported, so a loop can neither run forever merely
   because a provider is silent about cost nor stop at once merely because a
   ceiling was left unset.
3. **Each bound is checked on its own, in the order attempts, then time, then
   tokens, and the answer names the one that stopped the work** together with the
   figures it stopped at. "It stopped" is not a reason a human or a queue can act
   on, and raising the wrong ceiling is the wrong fix. The order is fixed so that
   two readers of the same three figures report the same reason, which is what
   lets a journal line and a TUI row disagree about nothing.
4. **A ceiling exactly met has not been exceeded.** `attempts > max` stops and
   `attempts == max` does not: a ceiling of three means three attempts were paid
   for and a fourth is refused. A ceiling of zero stops at the first refusal,
   which is how a project that allows no retries at all says so.

## Alternatives considered

- **`std::time::Duration`.** It loses on the crate's own rule that the deciding
  layer holds data, not clocks; and two duration types inside one bound would be
  a conversion every caller has to remember.
- **`max_tokens: u64` with zero meaning "unbounded".** Zero is also the figure a
  silent provider leaves behind, so one value would mean both "no ceiling" and
  "spend nothing": every run would stop at its first attempt as soon as a
  provider reported nothing.
- **Report every spent bound, or the worst one.** A list answers a question no
  caller asks, and "worst" needs a ranking between attempts, seconds and tokens
  that has no meaning. Fixing the ask order yields one reason, identically,
  everywhere.
- **Fold the bound into `Breaker`.** A breaker's state is a map from failure
  signatures to counts; it never sees a clock or a token figure. Merging them
  would make a test of the token bound set up signatures it does not care about,
  and this task's done-when — each bound trips alone, and names itself — is
  exactly the separation that keeps each bound testable by itself.

## Consequences

- A remediation now has an arithmetic stop that does not depend on what the
  failure was named, which closes the alternation blind spot a failure-signature
  breaker cannot see. Which bound stopped a run becomes one line a journal record
  and the TUI can carry without re-deriving it.
- Whoever asks the question converts the stopwatch. A caller that measured an
  attempt with `Instant::elapsed` holds a `std::time::Duration` and has to hand
  over a `time::Duration`; that one conversion is the price of keeping a clock out
  of the deciding layer, and the journal already files the instants a wall-clock
  figure can be recomputed from (ADR-0065).
- Nothing here reads a clock, a journal or a provider: `should_continue` is
  arithmetic over what the runner hands it. That is what let each bound be tested
  alone — the test that trips one shows the reason naming it while the other two
  sit inside their ceilings — and eight mutants of the comparisons, the ask order
  and the reason text were each caught by at least one of those tests.
- Nothing consumes `should_continue` yet. Asking it — the runner's per-attempt
  accounting of elapsed time and tokens, and where the three ceilings come from,
  most likely a `[remediation]` section beside the existing
  `circuit_breaker_threshold` — is later work, and is a decision of its own rather
  than a consequence of this one.
- If a provider ever reports tokens mid-session rather than at the end, decision
  2 needs revisiting: a figure that arrives during an attempt is a reason to bound
  inside the attempt, not only between attempts.

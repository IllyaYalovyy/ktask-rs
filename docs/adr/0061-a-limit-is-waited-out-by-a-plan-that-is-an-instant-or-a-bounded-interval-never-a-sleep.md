# 0061. A limit is waited out by a plan — an instant or a bounded interval, never a sleep

- **Status:** accepted
- **Date:** 2026-09-20

## Context

ADR-0060 ends by naming the gap this closes: `limit_message` returns the line a
limit was named on, and nothing read a reset out of it, so every limit was
waited out with bounded backoff. Closing it is a decision rather than a parser
for three reasons.

- **The wait has to survive the process that started it.** Recovery is one of
  the three properties VISION.md ranks above everything else: every transition
  is journaled before its side effect. ADR-0009 is the record of why `time` was
  adopted for exactly this — `docs/DESIGN.md` gives `PauseReason` a
  `Limit { until: Option<OffsetDateTime> }` so the journal holds an instant and
  a supervisor that dies mid-wake resumes the wait it promised rather than a
  shorter one. A function that sleeps cannot be journaled; a value can.
- **The two ceilings are all the configuration offers.** `docs/DESIGN.md` names
  `limit_wait_margin_secs = 60` and `limit_max_wait_secs = 86400` and no third
  knob, so the same numbers size the cushion beside a known reset and the pause
  taken for an unknown one.
- **A wrong parse costs more than no parse, and the two directions cost
  differently.** A reset read too far ahead parks a run that looks alive while
  it waits out a day for a limit that lifted in five minutes; a reset read too
  near asks a refusing endpoint again every second, which burns attempts and can
  trip the circuit breaker. Both are worse than the honest answer, which is that
  the line did not say.

## Decision

**Two pure functions, and the sleeping stays out of both.**
`classify::parse_reset(text, now)` reads an instant; `classify::wait_plan(reset,
now, margin, max)` answers with `WaitPlan::Deadline { at }` or
`WaitPlan::Backoff { wait }`. Neither reads a clock, a random number, nor a
file, and neither sleeps: the plan is what goes into the journal before the wait
begins. Both are re-exported from the crate root beside `classify` and
`limit_message`.

**Four shapes, in a fixed order, first one the line holds wins:** an absolute
day with an optional clock and offset; a clock time, resolved to the next day it
falls on; a duration measured from `now`; a bare `retry-after` number, which is
seconds. A day written with no time means its first instant, because that is the
boundary a daily window lifts on, and a clock or a day with no offset means UTC.

**Refusal beats inference, in four places.** A clock the line never made a
deadline (`at`, `by`, `until`, `till` in front, `UTC`/`GMT`/`Z` on the tail) is
not a reset — a line of output is full of clock times that say when a line was
printed. A clock followed by words that would place it (`pm`, a named zone,
`tomorrow`) is not a reset — this parser has no twelve-hour clock, no tzdata,
and no rule for whose Tuesday anything is. A day no calendar holds is not
re-read as the clock or the duration inside it. A duration written with no
separator (`1h30m`) is refused whole rather than half-honoured as `30m`. Each
refusal answers `None`, which `wait_plan` turns into a bounded backoff.

**A plan is always finite.** `Deadline` is returned only when the instant plus
the margin is strictly ahead of `now` and no further ahead than `max`; every
other case is a `Backoff` clamped into `0 ..= max`. A promise the provider
already missed is one of those cases: waking *at* an instant behind `now` means
waking now to be refused again. A proptest holds the invariant the caller is
entitled to assume — no plan is negative and none runs past the ceiling — so
whatever sleeps needs no watchdog of its own.

**Jitter is deliberately not in here,** although VISION.md §7 lists it beside
the margin. This function is pure and its answer is journaled; two calls with
the same arguments producing two different instants would make a resumed wait
indistinguishable from a changed policy. Whoever sleeps adds jitter around the
plan, never inside it. That is a deviation recorded rather than denied: §7 is
not fully implemented until the sleeping side does its half.

## Alternatives considered

- **Returning a `Duration`, or sleeping here.** A length is measured against the
  clock at the moment it is made, so a journal that holds one resumes into a
  wait whose remaining length depends on how long the process was dead. It also
  throws away the instant, which is the whole reason ADR-0009 adopted `time`.
- **Reading `OffsetDateTime::now_utc()` inside `parse_reset`.** `now` decides
  which day a clock time falls on and where every duration is measured from;
  taken from the system clock it makes both unreproducible, and the tests at
  23:59:30 and at 23:59:59 on 31 December would need an injectable clock anyway.
- **Defaulting a reset that cannot be read to `max`.** It is bounded in form and
  unbounded in spirit: the run sits for a day on a token nobody parsed, which is
  indistinguishable from a hang from outside.
- **Summing every duration on the line.** A line that names two waits ("try
  again in 20s; the window resets in 4h") has one answer, and adding them
  invents a deadline no provider sent.
- **Parsing twelve-hour times, weekday names, and named zones.** Each needs a
  database or a convention this repository has not chosen, and each fails by
  waiting the wrong number of hours rather than by failing.
- **Reusing the `Table` phrase machinery for the shapes.** The tables answer
  whether a line is a limit; the reset shapes capture groups and owe a date, an
  offset, and a length, which `Table` has no way to express.

## Consequences

- A provider that writes a reset in a new shape is a pattern edit beside the
  four, and every shape it refuses costs a bounded backoff rather than a hang.
- A caller that has to tell "no reset" from "a reset too far out" reads the
  `Option` it passed in: the plan does not say *why* it backed off. If the task
  that reports a pause needs that distinction, `WaitPlan` grows a reason —
  cheaper to add with a second consumer than to guess one now.
- `margin` does double duty as cushion and backoff length until a
  `limit_backoff_secs` key exists. Separate them when configuration is next
  touched.
- Nothing sleeps yet. `WaitPlan` is not wired to `PauseReason` or to a scheduler,
  so in practice a limit is still waited out by its caller's backoff. This is the
  deciding half of §7, not the sleeping half.
- Jitter exists nowhere in the workspace. The task that owns the sleep owes it,
  or §7's list stays half-implemented.
- A clock is honoured to the second: fractional seconds on an instant are
  truncated. A margin of a minute dwarfs the difference, and a reset written to
  sub-second precision is a machine's reset rather than a human's.

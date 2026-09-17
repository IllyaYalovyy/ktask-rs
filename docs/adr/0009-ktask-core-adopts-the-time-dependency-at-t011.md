# 0009. ktask-core adopts the `time` dependency at T011

- **Status:** accepted
- **Date:** 2026-09-17

## Context

`docs/DESIGN.md` defines `PauseReason` with one payload-carrying variant:
`Limit { until: Option<OffsetDateTime> }`. The reset time is not decoration —
it is what makes a wait survive a restart, since the journal holds the instant
and a supervisor that dies mid-wake resumes to the same wait instead of a
shorter one. So `PauseReason` cannot be written without `time`, and T011 exists
to write `PauseReason` before anything needs it.

The plan puts `time` elsewhere. T012's text reads "add `time` and `serde` to
this crate's dependencies", and T011's file list is `state.rs`, `classify.rs`
and `lib.rs` — no `Cargo.toml`. Neither task can be done exactly as written:
T011 cannot name a type its crate does not depend on, and T012 cannot add a
dependency the lock file already resolved.

ADR-0001 is the precedent for the opposite case, and it is the reason this had
to be settled rather than assumed: where a payload names a type *no task has
defined yet*, the field takes the underlying form, because defining someone
else's type would collide with the task that owns it. `OffsetDateTime` is not
that case. It is defined, versioned and feature-gated by `docs/DESIGN.md`
itself; it is simply not yet in this crate's dependency list. Substituting a
`String` for a type that exists would trade a resolved dependency for a parse
step and a lossy encoding, to avoid editing one line of `Cargo.toml`.

## Decision

`crates/ktask-core/Cargo.toml` gains `time.workspace = true`, and the resolved
`Cargo.lock` is committed in the same commit — the build gate runs `--locked`,
so an uncommitted lock file fails the next task rather than this one. No
version, feature or crate is added beyond what `docs/DESIGN.md` already fixes
in the workspace dependency set.

## Alternatives considered

- **`until: Option<String>`, per ADR-0001.** Rejected: the precedent is for
  types no task has defined. This one exists, and a `String` reset time invites
  every reader to parse it differently — the exact drift the vocabulary enums
  exist to prevent.
- **Define `PauseReason` without `Limit`'s payload, or omit the variant.**
  Rejected: the enum is specified exactly, and a `Limit` that cannot say when
  to wake is the difference between a bounded wait and an unbounded pause.
- **Let T012 add `time` and delay `PauseReason`.** Rejected: T011's whole
  outcome is that the vocabulary exists before the event catalog needs it;
  reversing the order breaks the task that consumes it.
- **Report NEEDS_INPUT.** Rejected: one `Cargo.toml` line, reversible, with the
  dependency list and its features already fixed by the design document.

## Consequences

T012 finds `time` already present and adds nothing; its `Cargo.toml` edit
becomes a no-op, and its own text stays accurate about `serde`, which was
already there. Anyone reading the plan should treat the dependency as adopted.

One consequence worth recording, because it surprised us and no task chose it:
with `time`'s features as `docs/DESIGN.md` fixes them (`formatting`, `parsing`,
`macros`, `serde` — no `serde-human-readable`), an `OffsetDateTime` encodes to
JSON as the tuple `[2026, 260, 12, 34, 56, 0, 0, 0, 0]` rather than an RFC 3339
string. It round-trips losslessly, which is all T011 requires, and the test for
it pins the decoded instant rather than the text. Journal storage and `--json`
output are a different story: a numeric tuple is unreadable to a human
inspecting a log, so whoever lands the JSON renderer or the `events` table
decides between adding `serde-human-readable` to the workspace `time` features
(an edit to the fixed dependency set, so its own ADR) and formatting the field
at the render boundary. Widening the features here was declined for the same
reason every other unasked-for change is: it changes what every later task may
assume.

# 0010. TddException categories come from VISION.md, not DESIGN.md

- **Status:** accepted
- **Date:** 2026-09-17

## Context

`docs/DESIGN.md` mentions `TddException` exactly once: the event catalog lists
`TddExceptionUsed` with payload `exception: TddException, reason: String`.
Unlike `Stream` and `Recovery`, which the same section defines below the table,
its variants are never enumerated anywhere in `docs/DESIGN.md` — and T011 is
told to define it "from the same document". Taken literally the instruction
cannot be followed: the document gives a name and no contents.

The contents are not actually missing. VISION.md §9, "The `tdd` protocol",
fixes them as a closed list: "Explicit exception categories (recorded in task
history): documentation, pure refactoring, build configuration, and bugs
already covered by a failing test." The same four recur as the exceptions to
test-first discipline everywhere the rule is stated — AGENTS.md's own exception
list, and every task prompt written from it. What is genuinely open is only the
spelling: four English phrases, and the Rust identifiers and JSON names to
match.

The type's purpose constrains that choice. A TDD exception is a permission a
task claims against an enforced rule, recorded so the override is visible in
task history instead of silent (VISION.md §16 names "loud recorded overrides"
as the mitigation for phase misclassification). An enum that let a task
describe its own reason would not constrain anything, so the set stays closed
and refuses anything else.

## Decision

`TddException` gets four variants in `classify.rs`, one per VISION.md category:
`Documentation`, `PureRefactoring`, `BuildConfiguration`,
`ExistingFailingTest`. Each doc comment quotes the phrase it stands for. As
with every other vocabulary enum in T011, the JSON name is the Rust name:
`docs/DESIGN.md` spells variants in CamelCase and fixes no `rename_all`, so
`"ExistingFailingTest"` is the encoding, and a test asserts both the count and
the refusal of any other name.

## Alternatives considered

- **Report NEEDS_INPUT.** Rejected: the set is written down in three places and
  no product decision is open, only spelling. ADR-0001 set this bar — a
  mechanical, reversible resolution is taken, not escalated.
- **Infer additional categories** (e.g. `ConfigOnly`, `GeneratedCode`) to make
  the enum look complete. Rejected: VISION.md's list is explicit and closed,
  and widening it weakens the rule the type exists to enforce.
- **Define it in `state.rs` with the other vocabulary.** Rejected: `state.rs` is
  where a run is and why it paused; `classify.rs` is why progress stopped, and
  an exception to a process rule belongs beside the failure classes.
- **Leave `TddException` out until a task owns it.** Rejected: T011's outcome is
  that the vocabulary exists before anything refers to it, and the event catalog
  is written against this name.

## Consequences

`docs/DESIGN.md` gains a gap worth closing: the event catalog refers to a type
it does not define, so a later documentation task should add the four
categories to it, citing VISION.md §9. Until then this ADR is the citation, and
`classify.rs` points at it.

T012 excludes `TddExceptionUsed` from `EventKind` on the grounds that its
payload type "no earlier task has defined" — written before T011 defined it.
That exclusion is T012's to make or drop; the type is now available at
`crate::classify::TddException` either way, and nothing in this task depends on
the choice.

`ExistingFailingTest` is the one name where the Rust identifier paraphrases its
category rather than repeating it, because "bugs already covered by a failing
test" describes a circumstance and its three siblings describe a kind of
change. If the phrasing ever matters to an operator-facing string, the `Display`
impl belongs to whoever builds the history screen, not to this enum.

# 0074. A test-first exception is declared in the body and claimed with a reason

- **Status:** accepted
- **Date:** 2026-09-21

## Context

§9 ends the `tdd` protocol with one line: "Explicit exception categories
(recorded in task history): documentation, pure refactoring, build
configuration, and bugs already covered by a failing test." Every other sentence
of §9 is enforced by a predicate someone can write — ADR-0071's `check_scope`,
ADR-0072's `verify_red`, ADR-0073's `verify_green` — and this one is not. Test-first
*order* is unrecoverable after the fact: no run over a green tree can tell a
test written first from a test written last. So the exception is the one part of
`tdd` the supervisor cannot check. That is exactly why it cannot be assumed.

Two halves make that concrete, and they pull apart.

The first is the task's. A task that genuinely is documentation cannot write a
failing test, and refusing it would push the author into writing a pointless one
to satisfy a machine — the outcome §9's sentence exists to avoid. But an
exception that is *inferred* from the shape of a diff is the agent's own account
of its own innocence, which is §3's invariant 4 refused in one line. So the
exception has to be asked for before the attempt, in words the author wrote.

The second is the journal's. `docs/DESIGN.md` gives `TddExceptionUsed` a payload
of `exception: TddException, reason: String`, and ADR-0011 kept the entry out of
the catalog until a task existed to write the `apply` arm that answers it. An
entry that moves a task nowhere is evidence, and evidence whose value is
entirely the reason beside it cannot be journalled with the category alone.

What makes this a decision rather than two functions is where the declaration
lives, what spells the four categories, and what a claim may not do. T011 put
`TddException` in `classify.rs` with the four §9 names as its variants
(ADR-0010), and that type is the enum's JSON encoding: the words in a task file
and the words in a journal row are the same four or they are two vocabularies
pretending to be one. The task brief for T079 named two shorter spellings for two
of them. And `docs/DESIGN.md` gives the queue's `tasks` table no column for a
declaration, while ADR-0019 fixes the body as the home for a fact the plan file
already states.

## Decision

1. **One vocabulary, in `classify.rs`.** `protocol.rs` re-exports
`crate::classify::TddException` rather than defining a second enum. The four
categories are spelled as ADR-0010 fixed them — `Documentation`,
`PureRefactoring`, `BuildConfiguration`, `ExistingFailingTest` — which are also
the type's Serde encoding, so one word travels from a task file through the
event to the journal row. `PureRefactor` and `BuildConfig` are refused as what
they are: words naming no category.

2. **The declaration is a `**Tdd-exception:**` section of the task body.** Its
first line is the category, matched exactly — no folding, no trimming, no
prefix — and everything after it is the reason, which is required. The section is
read by `task::section_of`, the one labelled-section reader that [`crate::gate_of`]
now shares, so the two rules a body and a row must never disagree about hold
everywhere: a label inside a fenced block marks nothing, and a label written
twice keeps the text written under it first.

3. **The skip happens when the protocol is chosen, and shortens the body only.**
`for_task` resolves the protocol word exactly as it would without a declaration
and then removes `Phase::Red` from the resulting phase list. `Protocol::name` is
untouched, because it is the word `AttemptStarted` journals and the shape an
attempt is replayed against; the appended completion pair is untouched, because
`assemble` put it there and §9's constitution forbids removing a gate. A runner
walks the order it always walks and finds one fewer phase to enforce — no
exception-shaped branch in the runner.

4. **A declaration that cannot be honoured is refused at the door, under key
`tdd_exception`.** No category named, nothing written under the label, a category
with no reason, and a claim against a protocol with no red phase are all refusals
raised by `for_task`, for the reason ADR-0070 gives a bad protocol word: a task
that cannot be worked should cost its attempt nothing. A *section* that is absent
is not a refusal — that is the ordinary task, and the skip is worth nothing if it
is the default.

5. **A claim re-runs the phase's own scope check.** `claim(task, scope, changed,
test_globs)` reads the declaration, then calls `check_scope` with the same two
arguments the phase's gate is given, over the paths `git::changed_paths` named. A
phase that wrote production code during what should have been red therefore has no
exception left to claim: the refusal quotes `SCOPE_RULE` or `UNLOCATABLE_RULE` and
names the paths, and `classify` lands it as `PolicyFailure`, which earns no retry.
An exception is not a repair for what was already written.

6. **`apply` admits the event in the two states an agent works in, and moves
nothing.** `TddExceptionUsed` is a self-transition from `Running` and from
`Remediating` — evidence, like `AttemptRecorded`, and `Remediating` needs it
because an exception task's second attempt still works phases. The other six
states refuse it. Its two rows are the only additions to `LEGAL`; the entry
carries no attempt, commit or phase, so unlike `AttemptRecorded` and
`PhaseEntered` it has a row on both sides of the payload-sensitive sweep.

## Alternatives considered

- **Define a second `TddException` in `protocol.rs` with the brief's spellings.**
It compiles, and it splits one concept in two: `EventKind::TddExceptionUsed`'s
payload has to pick one of the two enums, so either the journal encodes
`classify`'s words and the task file speaks others, or a task's word is translated
at the boundary — a translation table between two names for one thing, which is
the thing ADR-0010 was written to avoid. The brief's two shorter spellings are
reported as a finding against the brief, not honoured in code.
- **Infer the exception from the diff** — no test paths touched, therefore
documentation. Rejected outright: it is the agent's account of its own innocence,
and it also gets every real case wrong (a documentation change that edits a
doc-comment inside a compiled file, a build configuration change that touches a
`build.rs` an agent wrote). The declaration is what makes it *recorded* rather
than *assumed*, which is the outcome's whole sentence.
- **A `tdd_exception` column on `tasks`.** Rejected by ADR-0019: the plan file
states the exception, the row holds what was asked, and a column that duplicates
a body section is a second copy that can disagree — with a queue rewrite the only
way to find out.
- **A `**Protocol:** tdd-no-red` word, or a third `tdd_exception()` constructor.**
§9 names two protocols v1 runs and ADR-0068 refuses a workflow an attempt could
assemble; a protocol whose only difference is "we skipped the phase we were
supposed to skip" also renames the attempt, so a replay would report a protocol
the task never asked to be worked under.
- **Accept `TddExceptionUsed` from `Verifying` and `Publishing` too,** where
`AttemptRecorded` is accepted. Rejected: the exception describes a phase, and
those states hold no phase. A claim that arrives after the attempt stopped working
is the after-the-fact override, which is the abuse the entry exists to prevent.
- **Let the reason default to empty.** The payload has a `reason` field precisely
because a category with no reason is unauditable; defaulting it would make the
journal record an override nobody can judge, and the screen would render a blank
where the judgement belongs.

## Consequences

- A `tdd` task that is documentation declares it and is worked as
`[green, refactor, verify, publish]` under a `tdd` attempt. Nothing about the
runner changes to accommodate it: the phase list it reads is one shorter.
- Two refusals now live in configuration where they belong (`key:
  "tdd_exception"`) rather than in an attempt's failure, so an author sees
  "your section names no category, here are the four" before a session is paid
  for. The task's protocol word is settled first, so a row wrong in both ways is
  refused for the first and not told about the second.
- The refusal of `PureRefactor`/`BuildConfig` means the brief's spellings fail
  loudly rather than silently becoming a second encoding. If a real plan file
  ever uses them, the fix is the plan file, and the refusal text already names
  the four accepted words.
- `task::validate` does not yet ask a declaration of a task as it is added, so a
  task carrying `**Tdd-exception:** SpecsOnly` reaches the queue and is refused
  when its protocol is chosen. That is one refusal per task rather than a hole —
  `for_task` cannot be bypassed — and wiring it into the add path belongs with
  the queue commands, not here.
- `docs/DESIGN.md` still lists only the payload type, not the four categories
  (ADR-0010's standing consequence). The categories are named in `classify.rs`
  and in this file; a third spelling in `docs/DESIGN.md` would need its own
  decision about which document is authoritative for the list.
- Nothing calls `claim` yet, as with `check_scope`, `verify_red` and
  `verify_green`: the runner that walks a protocol's phases wires the four
  together and journals what they return, before the phase's next side effect —
  which is where §6's "every transition is journaled before its side effect" is
  decided for this event as for the others.

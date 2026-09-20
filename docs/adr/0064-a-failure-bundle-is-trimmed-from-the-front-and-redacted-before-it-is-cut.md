# 0064. A failure bundle is trimmed from the front, and redacted before it is cut

- **Status:** accepted
- **Date:** 2026-09-20

## Context

VISION.md §7 requires that every remediation launches a *fresh* provider
session seeded with a compact failure bundle — classification, gate output, diff
summary, prior attempt evidence — and that session resume is never relied on,
because determinism outranks token economy. T069 fixes the door,
`bundle(task, class, gates, diff_summary, prior, budget_bytes) -> String`, and
three properties it must hold: one failure produces a byte-identical bundle, the
bundle never exceeds its budget, and no secret survives it. Four decisions are
not made by the task body.

- **Compact fights unbounded.** A `GateResult` keeps its whole transcript on
  purpose (VISION.md §8), a task's history grows by a record every remediation,
  and an agent's exit reason is free text. Left alone, a bundle grows with the
  failure it describes and spends the context the fresh session needed.
- **Evidence fights determinism.** The journal is deliberately full of when and
  where: durations, instants, session and model ids. All four move between two
  runs of one failure, and the done-when asks for the same bytes twice.
- **Truncation fights redaction, and the order is the whole question.**
  ADR-0032's table matches *shapes*, and a cut can land inside a shape.
- **A fresh session cannot ask.** It has no journal, no git tree, no gate output
  and no memory of the attempt it is repairing; everything it needs has to be
  in the text, or counted there.

## Decision

**Blocks are laid down oldest first and rendered newest-first.** The order laid
down is the prior attempts by their own number, then this attempt's diff
summary, then the gates that refused in the order they ran, then the frame that
names the task and the class. Trimming takes from the front of that order, and
the bundle is rendered with the newest block first. One rule serves both:
*write what a session needs most last, and shed from the front*. Its useful
consequence is what a partially-fitting block keeps — the *tail*. The last lines
of a refusing command are its verdict and the first are its progress, and the
newest attempt is the one a repair must not repeat.

**The frame is laid down last, and it counts what a budget dropped.** A bundle
trimmed to a handful of bytes is left holding `class: …`, which is the one line
that decides the response (VISION.md §7 fixes a response per class); and the
frame's counts — how many prior attempts there were, and which kinds of gate
refused — survive when the attempts and the gate labels beneath them do not, so
a session handed a third of the evidence cannot conclude that there was a third
of it.

**Redaction happens before the cut, never after.** A cut can land inside a
secret, and half a shaped secret is a shape the table no longer recognises: a
value halved by a knife is exactly the case a shape matcher cannot see. Redacting
first means the worst a cut can do is halve a `[redacted]` mask, which leaks
nothing. This is an ordering constraint on the implementation, not a filter that
can be moved: the tests price every budget from zero upward rather than sample
two.

**Nothing time-shaped and nothing session-shaped goes in.** No stopwatch
duration, no wall-clock instant, no session or model id, no base sha. They stay
in the journal, which is where a run's timing is read from, and the bundle names
the task and the attempt numbers, which is the key into it. Determinism here is
a property of what the bundle is *about* — what happened, not when — rather
than a comparison that forgives a moving field, which is the route ADR-0062 had
to take for a signature.

**`budget_bytes` is a ceiling in bytes, and a cut lands on a character.** The
budget is the byte length of the string returned, `0` answers with an empty
bundle, and a cut that would split a character backs off to the nearest boundary
it can reach. Only when the whole remaining budget is smaller than the first
character of a line does the cut overshoot, by less than one character, and the
character goes rather than the ceiling being spent.

**Satisfied gates contribute nothing, and absent evidence is stated rather than
skipped.** A green gate's chatter is not failure evidence and spends the budget
a refusal needs. Where evidence is absent the bundle says so in words —
`produced no commit`, `usage unasked`, `usage unreported`, `the gate refused
without writing anything`, `no diff summary was given` — because a zero
substituted for an unreported figure tells the session a fact nobody reported,
and `usage unasked` and `usage unreported` are different facts about a run.

**A prior attempt gets one line**: the kinds it refused, the commit it produced,
what its session spent, and the sentence it stopped with. Those four are what
stops a remediation re-running a fix that already failed; its transcript, its
session id and its clock are not.

**The built-in redaction table is the only table a bundle has.** The signature
T069 fixes carries no `secret_patterns`, so configured shapes cannot reach a
bundle from here without widening a signature another task is already written
against. A project whose keys have a shape of its own redacts them where the
bundle's inputs are read. This gap is reported rather than quietly closed.

## Alternatives considered

- **Truncate, then redact.** The obvious order, and the one that leaks: the cut
  is placed by the budget without knowing what it is cutting through.
- **Keep the head and cut the tail**, the conventional `…` truncation. Rejected
  because it keeps the part that carries least: a command's opening progress and
  the oldest attempt, and drops its verdict and its newest attempt.
- **A fixed per-gate tail** (last forty lines, say). It needs a second budget
  beside `budget_bytes`, and a fixed count is wrong at both ends — it discards
  half of a five-line refusal and most of a five-thousand-line one. The budget
  already says how much evidence fits; the frame says how much there was.
- **Keep the instants and normalise them away**, the way the signature strips
  timings. A signature must be *compared*, so it has to forgive a moving field;
  a bundle only has to be reproducible, and leaving the moving fields out is
  simpler than writing them down and discounting them.
- **Leave prior attempts in the order the caller gathered them.**
  `attempt_records` files evidence as each attempt's recorder reaches it, so one
  failure gathered in another order would render a different bundle.
- **Return a `Bundle` newtype, or `Vec<u8>`.** Better typing at the call site;
  T069 fixes `-> String`, and the runner passes the result straight into an
  invocation prompt.

## Consequences

- `bundle` is a pure function of its arguments: no clock, no randomness, no
  journal, no git. Byte-identical reruns are what the function *is*, not a
  cache it maintains, so the done-when is checked by an equality test rather
  than asserted about timing.
- Shed order is testable without reading the implementation. The test suite
  *prices* each marker — the smallest budget that still holds it — and asserts
  the order those prices come out in. A mutant that sheds in another direction
  moves a price.
- A trimmed transcript arrives unnamed: a block's label is the oldest line of
  its block, so the first thing a tight budget takes is the line that said which
  gate wrote the rest. `gates refused: …` in the frame is what says who a
  nameless tail belongs to.
- Nothing in this crate calls `bundle` yet. Gathering the diff summary and the
  prior attempt records, and handing the result to a fresh session, belongs to
  the runner task; the redaction gap above belongs to whoever widens the
  signature.

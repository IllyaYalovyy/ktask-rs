# 0075. The assembled prompt is a pure projection that names its report below the state directory

- **Status:** accepted
- **Date:** 2026-09-21

## Context

VISION.md §6 says task context "is assembled by the runner, never hand-injected
per task: in v0.1 it is a static context document from the private prompt library
plus every ADR recorded so far". T080 fixes the shape of that assembly with a
signature that is already written: `assemble(task, context_doc, adrs, template,
attempt, total) -> String`, handing over a `String` that
`Invocation` carries to an adapter as the whole of what a session is
told. Four forces make the body of that function a decision.

**Determinism is the point of the function, not a property of it.** §7 ranks
determinism above token economy, and an attempt is only comparable with the one it
remediated if the words it was started with can be produced again. The three
things a prompt could still be built from that are not inputs — a clock, the
environment, the filesystem — are precisely the three that make the same task
produce different bytes on Tuesday. §6's own v0.2 sketch (a size budget over a
VISION excerpt, relevant ADRs and prior resolutions) is only writable against an
assembly that reads nothing.

**Privacy is by construction, and the prompt is the leak most likely to be
written on purpose.** The header tells an agent where to write its report. §11
keeps every operational artifact outside the repository, and §3's invariant 6 is
the rule; but the signature carries no `Project`, so nothing here knows
where a project's state directory *is*. A path resolved from `$XDG_STATE_HOME`
and the process's home directory would reach outside the inputs, and a bare
relative path would be resolved by the agent against its working directory —
which §10 makes the task's own worktree, so the report would land inside the
repository, mode 0644, and be caught by the privacy scan as an AI artifact the
run wrote on purpose.

**The report is evidence, and the agent's account of it is not.** §3's invariant
4 refuses to call a task done on an agent's statement, and ADR-0065 already gave
an attempt's directory a `report.md`: the record *the runner* generated from the
gates and SHAs it watched. An agent's own report is a second and different thing,
and naming both the same file is how one comes to stand in for the other.

**The ADRs are the only part a caller read from inside the repository**, because
a recorded decision is the one operational document §3 lets live there. Everything
else in the prompt comes from the private prompt library or from the queue's own
row. The done-when asks that this be visible in what the function does rather than
in a comment, which is what makes "reads nothing" testable at all: an input list,
never a directory walk.

## Decision

**Four parts, always in this order, joined by a blank line**: the header, the
context document, the decisions, the template with the task in it. The order is
§6's own list and the order a reader needs — who is asking, what the project is,
what has already been decided, what to do.

**The header names the task number, the queue's length, the attempt, and the
report path.** `total` is the number of tasks the queue holds, printed as
`task 12 of 161`: it is the one figure about the queue a prompt can carry without
a database in scope, and it is not derivable from anything else in the signature —
`adrs.len()` already says how many decisions there are, so a second reading of
this argument would have been a redundancy the type could not catch.

**The report path is `<state_dir>/attempts/<task>/<attempt>/agent-report.md`.**
Three choices in one string:

- the two id levels are the ones `evidence_dir` uses, so an agent's report
  joins its own attempt's directory rather than a third layout nobody reads.
  `attempt::EVIDENCE_ROOT` became `pub(crate)` rather than be spelled a second
  time in a second module: two spellings of one layout is how a prompt and a
  directory end up disagreeing;
- `agent-report.md`, not `report.md`, which ADR-0065 gave to the runner's own
  generated record. The two reports of one attempt say different things and
  VISION.md §3 keeps them apart;
- `<state_dir>` is a **marker**, not a resolved directory, and it is spelled out in
  the header line rather than left to be inferred. It is not a valid first path
  component, so an agent that tries to write there is refused by the filesystem
  and says so in its report. That refusal is the chosen failure mode: the
  alternative that "worked" silently put a run's report in the repository.

**Nothing is read.** No clock, no environment lookup, no path opened. The ADRs
arrive as text, having been read by the caller; the assembly is a projection, in
that respect exactly like `bundle`. Two calls over equal inputs are equal
bytes, which is asserted twice over: once against the whole prompt spelled out
line by line, once as a property over arbitrary inputs.

**Trailing whitespace goes, and nothing else.** Each part loses the newline it
picked up from the file it was read out of, so a context document ending in `\n`
does not open a gap where the next part starts; a part that holds nothing is
dropped rather than printed as a heading over an empty body. Nothing inside a part
is reflowed, escaped, trimmed at the front, or cut — indentation inside somebody's
code fence is their text, and a prompt that quietly edited the context document is
one an operator can no longer compare with the file.

**Every `{{TASK}}` is replaced, not the first.** A template may name the task
twice, and a placeholder left standing is a literal `{{TASK}}` in front of a
provider.

**A template that named no placeholder gets the task appended** under a `# The
task` heading. The alternative is handing a provider a prompt that asks for
nothing, and this function answers `String`, so refusing is not on the menu: a
template that reached here has already been read from the prompt library, and the
runner's only remaining honest move is to hand over something whole while the
defect is reported elsewhere.

**The decisions are numbered out of the total** (`## 2 of 7`) under a heading that
counts them, and an empty list reads `# Decisions on record (0)` over `None
recorded yet.` The order is the caller's, unchanged: ADRs supersede each other by
number, so sorting them by title would hand a session its project's history
upside down.

## Alternatives considered

- **Take a `Project` (or a report path) and print an absolute path.** The honest
  answer to the marker, and refused only because T080 fixes this signature and
  widening it here would change what every other caller compares. It is the first
  thing to revisit, below.
- **Name the report path relative to the worktree** (`attempts/12/2/report.md`).
  Rejected outright: an agent's cwd is its task worktree, so this is the one
  spelling guaranteed to write operational state inside the repository — §11's
  prohibition, arrived at by an agent doing exactly what it was told.
- **List the ADRs by path instead of embedding their text.** Shorter prompts, and
  the agent can open the files. Rejected because §6's v0.1 context is "the static
  context document plus every ADR recorded so far", which is a body of text, not a
  bibliography; and because an agent that skipped reading them would be behaving
  exactly as well as one that read them, which makes the injection unenforceable.
  The size budget that makes truncation explicit is §6's v0.2.
- **Let `total` be the number of ADRs.** It would have been `adrs.len()` in a
  different costume, and the queue's length is the figure the header cannot get
  any other way.
- **`insta` snapshots for the whole-prompt assertion.** Rejected: it adds a
  dev-dependency, which takes an ADR of its own, and a snapshot that nobody reads
  accepts any change. The array of expected lines in the test *is* the spec, and it
  fails loudly.
- **Return `Result<String>` and refuse a template with no placeholder.** The
  signature is fixed, and the refusal would have paused a queue over a typo in a
  template nobody was watching.

## Consequences

- The prompt an attempt was started with can be produced again from the queue's
  row, the context document, the ADR list and the template — which is what makes
  `context.md` in an attempt's directory worth reading three tasks later.
- A prompt that names `<state_dir>` names a path that cannot be opened. The task
  that starts a provider and owns a `Project` has to resolve it, and
  doing so inside `assemble` is the way to keep the recorded prompt and the
  launched prompt one string: widen this signature with the attempt's report path
  (or the project) and delete the marker. Until then the runner reads
  `agent-report.md` out of `evidence_dir` itself, and an agent that
  cannot write the named path says so in its report rather than writing it into
  the worktree.
- A project with a hundred ADRs gets a prompt with a hundred ADRs. §6 puts that
  under a size budget in v0.2; nothing here pretends to trim what it was handed.
- `attempt::EVIDENCE_ROOT` is now `pub(crate)`, so the layout has one spelling and
  a second module that changes it has to change the same constant.

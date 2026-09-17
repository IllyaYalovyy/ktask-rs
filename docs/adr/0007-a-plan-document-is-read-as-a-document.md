# 0007. A plan document is read as a document, not as a format

- **Status:** accepted
- **Date:** 2026-09-17

## Context

`parse_plan` turns a Markdown file into queue entries, and every previous
implementation of this idea in this project's lineage did it the other way
round: the queue file was a *format*. The supervisor rewrote it in place,
prefixing `[DONE]` and `[FAIL]`; `#` lines were comments and a line of three
dashes was a task separator, so no task could contain a shell comment or a
Markdown rule without breaking the queue. That is why the shipped task list
carries a warning at the top of its own file.

VISION.md §4 reverses it. A plan file is an **input format**, imported once;
the queue then lives in the database, nothing is rewritten in place, and the
document "is a document — ordinary Markdown with headings, no rules about what
a line may begin with, and no separators load-bearing enough to break it". A
person reads that file in a Markdown viewer, and a Markdown viewer renders a
`##` as a heading, a `---` as a rule, and the contents of a fence as code.

Four things follow from that and had to be settled where they would be read,
because each one is a rule a later task would otherwise re-invent:

- What divides a task, when a heading, a rule and a fence line are all just
  text that may appear inside a task.
- Where a human gate is recorded, given that `TaskStatus` describes what the
  supervisor concluded and a freshly imported task has been concluded on by
  nobody.
- What happens to a block that lacks a required section, given that a malformed
  task must never enter the queue (VISION.md §4) but `plan lint`, which reports
  every problem in the queue, is a later task.
- What `Task::body` holds, since the queue list title is a projection of it
  (ADR-0006).

## Decision

**The document is parsed as a document, and the block is kept byte for byte.**

- **A task is a level-two heading and everything until the next one.** Only
  `## `, at most three spaces of indentation, opens a task: `#` and `###` are
  headings of the *document* and stay inside the block above them, and
  `##NoSpace` is prose, both because CommonMark says so and because a parser
  that reserved `##` with no space would reserve a line a person can write.
  Everything before the first such heading is a preamble, and a preamble is not
  a task to queue.
- **A fenced code block hides its markup.** A fence opens on three or more
  backticks or tildes and closes on the next run of the same character that is
  at least as long and carries nothing else. Inside it a `##` opens no task and
  a `**Label:**` opens no section, so a task may quote the very format it is
  written in. Outside it, `---` is a rule and nothing more: no line is reserved.
- **`Task::body` is the block verbatim** — heading line, blank lines, fence
  delimiters and the file's own line breaks, `\r\n` included. The four required
  sections are held separately, trimmed only of the whitespace that separates a
  section from its label and from the next one, joined with `\n`, and their
  Markdown is kept: a `**Verify:**` written as `` `cargo test` `` keeps its
  backticks, because deciding what is a runnable command is not a parser's job.
  A label written twice keeps the first text written under it.
- **A `**Gate:**` section marks a human gate, recorded as `Task::gate`.** The
  section says what the person is being asked to decide, so its text is kept
  rather than reduced to a flag. Status stays `Pending` for every imported task
  — gate included — because `TaskStatus` is what the supervisor concluded, and
  an import concludes nothing.
- **A block missing a required section fails the import**, naming every section
  it lacks and the line its heading sits on. `Error::NotFound` carries that
  message: the `Error` enum is fixed by docs/DESIGN.md (see ADR-0001), a
  missing section is literally what `NotFound` says, and the CLI already maps a
  missing subject to the usage error that `docs/CONTRACT.md` assigns a
  malformed task file.
- **Ids are assigned from one in document order.** A document with more blocks
  than a `u32` can number is refused rather than wrapped or truncated, since a
  duplicated or clipped id is exactly the silent corruption the queue exists to
  prevent.

## Alternatives considered

- **Keep the old in-place markers.** It was the previous behaviour and it is
  cheap. Rejected on the rule above: it makes the supervisor the author of a
  document a person also edits, so a `[DONE]` sweep lands in the commits under
  review and a status edit can disagree with the journal.
- **A full CommonMark parser.** Correct for `## Title ##`, setext headings,
  backslash escapes and nested blocks. Rejected for this task: it needs a
  Markdown crate, which is a dependency and therefore an ADR, and the residue
  is a title keeping its trailing hashes and its own line break — noise in the
  error message, not in the queue. Revisit if a real plan document ever needs
  it.
- **Split on `##` anywhere, fences ignored.** Two lines shorter, and it breaks
  the one task type that matters here: a task that quotes the plan format, which
  this repository's own task list does constantly.
- **`is_gate: bool`.** Marks the gate with less, and throws away the question.
  The inspector and `ack` both need what is being asked; with a flag they would
  re-read the body to find it.
- **`status: HumanGate` at import.** It reads well until a run: `HumanGate` also
  means *the queue is paused here*, so a runner would either start a gate task
  it should stop at or stop at a task it should run. T097 needs "is this a gate
  task" and "is the queue paused at a gate" to be different facts.
- **Parse leniently and let `plan lint` complain.** It keeps `parse_plan`
  total, but `add --file` would then write a block with no `Verify:` into the
  queue, where the next task runs it. Rejected: a malformed task never enters
  the queue.

## Consequences

- Importing a real plan document is now safe: an `## Background` or `## Notes`
  section is a task and is *rejected* for lacking its sections, so a stray
  heading is a loud failure at import rather than a junk row in the queue.
  Someone writing a plan learns to keep prose out of level-two headings.
- `Task::title()` (ADR-0006) is the first line of `body`, so an imported task
  is listed as `## T008 Plan document parser` — with the hashes. That is the
  honest projection of a body that keeps its heading; a frontend that wants the
  label without the marker strips it where it draws, which is where the ADR
  already put that kind of decision.
- The `tasks` table in docs/DESIGN.md has columns for the four sections and for
  `body`, and none for a gate. Whoever writes the queue storage either adds one
  or re-derives `gate` from `body` on read; both keep the schema's own rule that
  nothing but the journal is authoritative. This is reported as a gap in the
  task that found it, not fixed by a parser task.
- `plan lint` still needs a per-task check over tasks already in the database,
  where no parse happens; the section-presence rule in this file is what it
  should call rather than restate.
- Import rejects at the first offending block, so a document with three
  malformed tasks reports one of them per run. `plan lint` is where "one line
  per problem" lives; `add --file` reads one task at a time and sees the whole
  message immediately.

# 0076. A report is a claim read from its first non-empty line, and never overrules a gate

- **Status:** accepted
- **Date:** 2026-09-21

## Context

T081 fixes both halves of the signature — `pub enum ReportResult { Done, Failed,
NeedsInput }` and `parse_report(text: &str) -> Result<ReportResult>` — and the
three words the header may be. What the task cannot fix is the three readings
that decide what the signature means in practice.

**Whose words these are.** The report is the agent's own file, the
`agent-report.md` the prompt's header tells the session to write (ADR-0075) and
not the `report.md` the runner generates from the gates it watched (ADR-0065).
`.ktask/prompt.md` states the rule the parser enforces from the other side: the
report's "first line is exactly one of `KTASK_RESULT: DONE`, `KTASK_RESULT:
FAILED`, `KTASK_RESULT: NEEDS_INPUT`".

**The header is the highest-stakes three words in a run, and the text around it is
written by an agent.** A session quotes its own prompt, echoes a template, or
prints the header under a heading it thought was decorative. Reading a claim that
was never made is the expensive direction to be wrong in; refusing a report that
was written badly costs one retry and a readable error.

**The done-when asks that a report claiming `DONE` never overrides a failing
gate.** VISION.md §3's invariant 4 already says a task is never done on an agent's
exit code or statement, and invariant 7 gives completion to three facts outside
the report. ADR-0059 built the ordered classifier that reads a session's
`KTASK_RESULT: NEEDS_INPUT` line *below* a refused gate. So the requirement is not
new — the question is how a module whose whole job is parsing keeps it true.

**The refusal has to be actionable.** A report nobody can read is an attempt that
spent tokens and produced nothing a screen can show. Whoever has to fix it needs
the line that was found and the lines that were wanted, in the same message.

## Decision

**The header is the first line with content, and only that line.** Whitespace-only
lines above it are walked past — an opening blank line carries no claim, so it
cannot be the header — and the line found is trimmed at both ends, because the two
things most likely to surround the header are the line ending the report was
written with and an indent. What follows it is prose and is not read: the first
line wins, so a report cannot claim two answers.

**The three spellings are matched exactly**, case, spacing and punctuation
included. `KTASK_RESULT:done`, `KTASK_RESULT: DONE, mostly` and
`ktask_result: done` are all refused. Leniency here buys nothing that a loud
refusal does not buy more cheaply, and the three words decide whether a human is
poked, whether a queue moves, and what a screen says about an attempt.

**The classifier's looser reading stays as it is, on purpose.**
`classify::NEEDS_INPUT` is case-insensitive, tolerates any spacing after the colon
and anchors at the start of *any* line, because it reads a session transcript
after something already went wrong and missing a genuine ask-for-input is the
costlier miss. This parser reads the filed report, whose location and first line
were dictated by the prompt, and the contract is the strict one. The two strictness
levels are a difference of evidence, not an accident to be reconciled: change one
and look at the other.

**The claim is inert.** Nothing in `report.rs` reads a gate, a SHA, a journal or a
clock, so a parsed `Done` has no path into a decision the gates did not make. One
test pins the pair anyway: a report claiming `DONE` over a refused `verify` gate
is read as `Done` *and* classified as a `VerificationFailure` in the same breath,
which is the invariant stated as behavior rather than as a comment.

**Refusal is `Error::Corrupt` with no journal position.** A report is durable text
filed in the project's state directory that could not be trusted, which is what
that variant is for. The detail names what the report opened with — the line
quoted, or the word `nothing` — beside all three expected lines, and those three
come from the same table the matcher consults, so a refusal cannot advertise a
header the parser would not have accepted.

## Alternatives considered

- **Find the first line that *starts with* `KTASK_RESULT:`.** The most forgiving
  reading, and refused: it is also the reading that turns a report's prose about
  the protocol into a use of it. ADR-0059 anchored its own table at the start of a
  line for exactly this reason, and a run that paused on every echo of the prompt
  would never finish.
- **Tolerate case, spacing or trailing words.** Rejected above; the short version
  is that every tolerance added here is a way for the wrong result to be read
  silently, and every refusal is a message that says how to be right.
- **Read the last header, or all of them, and require agreement.** A report with
  two different claims is a report whose structure is broken; picking either line
  by policy invents an answer. The first line is the one the prompt named.
- **Treat a missing header as `Failed`.** Rejected outright. An unreadable report
  is not a claim of failure; folding the two together turns a formatting mistake
  into a task failure and, worse, makes the mistake invisible.
- **Return `Option<ReportResult>`.** The done-when asks the refusal to name what
  was expected, which an `Option` cannot say.
- **Add an `Error::Report { .. }` variant.** The honest name for the failure, but
  T081's write scope is the parser and `lib.rs`, and `Error::Corrupt` already says
  "durable data was read and could not be trusted" with room for the detail. If a
  report error ever needs its own fields — the attempt it belongs to, the byte
  offset — that is a variant with an ADR, not a variant slipped in here.
- **`impl FromStr`, `Display`, or `Serialize` on the enum.** Nothing consumes them
  yet. The claim becomes a journal payload in the task that files an attempt, and
  a format nobody reads is a format nobody reviews.

## Consequences

- A report written on a host that ended its lines with `\r\n`, or indented under a
  bullet, is read. A report with a *decorated* header — a case changed, a word
  added, a space missing — is refused with a message quoting the line it read and
  spelling out the three it wanted.
- Opening the file stays the caller's job. `parse_report` is given text, so a
  report that was never written is an I/O failure rather than a malformed one, and
  the two keep meaning different things.
- The claim is recorded and never obeyed. If a future task lets a parsed
  `ReportResult` reach the state machine — a `Done` that starts publishing, say —
  it reopens this ADR, because that is invariant 4 being traded away rather than
  implemented.
- `classify` and `report` now hold two different strictness readings of the same
  three words. Both are deliberate; the pair is the thing to re-read if the report
  contract ever changes.

# 0079. A pause carries the question the agent wrote, in five plain labels

- **Status:** accepted
- **Date:** 2026-09-21

## Context

VISION.md §3's invariant 8 makes an unresolved product or technical decision a
first-class pause state rather than something an agent guesses its way past, and
§6 says what that pause holds: `waiting_input` is "the mechanism behind invariant
8: the agent (or a gate) surfaces a structured decision request (question,
options, trade-offs, impact); the queue pauses; `ktask-rs resolve` records the
answer as an ADR". `docs/DESIGN.md` fixes the type — `DecisionRequest { question,
options: Vec<String>, tradeoffs, impact, recommended: Option<String> }` — and the
catalog entry that carries it, `DecisionRaised { request: DecisionRequest }`. T084
emits that entry, and the only place a question can come from is the report an
agent files.

**The body of that report is prose nobody has prescribed.** ADR-0076 made the
header the claim and left everything under it unread; ADR-0075 fixed the file's
path and nothing about its shape. `.ktask/prompt.md` fixes a report's first line
and says nothing about the rest, and the report contract every session is handed
asks for four prose lines — `Summary:`, `Action:`, `Reason:`, `Evidence:` — none
of which is a question. So this grammar is being written before the prompt that
will use it, which is why it is a decision and not an obvious step.

**The two ways to be wrong about a pause are not equal.** Reading a `DONE`
report as a question stops a queue on prose that never asked, and a human poked
for nothing learns that this supervisor pauses on words. Reading a
real question as nothing is worse again — the blocker disappears, the attempt is
filed as finished, and the work runs on across a decision nobody made. Both
directions are refused below, and the asymmetry decides which one is an error
rather than an absence.

**A pause with no question in it is worth nothing.** `docs/CONTRACT.md` gives
`resolve` one job — it "Answers a `waiting_input` question" — and that answer
becomes an ADR injected into every later prompt (ADR-0078). A human handed
`NEEDS_INPUT` and no question has nothing to answer and no way to say so except
out of band, which is what settles whether a header claiming a pause over an
empty body is a wait or a broken report: VISION.md §3's invariant 4 is the same
rule seen from the other side, that the supervisor proves and a claim it cannot
read proves nothing.

**Where the question can live.** `TaskState::Paused` holds a reason, an optional
instant and the state it resumes to (ADR-0026), and it refuses a second pause;
growing a field per reason makes every reason pay for the one that has a payload.
The journal record is the one place that already carries a payload per event —
ADR-0016 made that payload the encoding of the catalog entry itself — so the ask
fits where the answer will be read.

## Decision

**Five labels, plain `Label:` at the start of a line**: `Question:`, `Options:`,
`Trade-offs:`, `Impact:`, `Recommended:` — the five names `docs/DESIGN.md` gives
the payload, first letter up, own colon, matched exactly. A report body is plain
text whose header is already `KEY: value` (ADR-0076), so its sections are written
the same way. The `**Label:**` shape `crate::task` reads belongs to a plan
document, which is Markdown a human also reads: two documents, two grammars, and
neither parser reads the other's file.

**Four of the five are required, and they are the four fields that cannot be
empty.** `question`, `options`, `tradeoffs` and `impact` are neither `Option` nor
empty-able on the struct, and a label written with nothing under it fills nothing,
which is the same shortage as never writing it. `Recommended` is optional on
purpose, and an empty `Recommended:` is the absence of a recommendation rather
than a recommendation of nothing: an agent at a genuine fork is not obliged to
have chosen already.

**`Options:` is a list, and a marker is not part of the choice.** One option per
non-blank line, with one leading `-`, `*`, `+`, `1.` or `1)` stripped — the
number only when a space follows it, because `1.x` is a version. Whether the
section counts as filled is asked of the parsed options, not of its raw text, so
a bullet with nothing after the dash names no choice and is refused.

**A `NEEDS_INPUT` short of a required section is a malformed report, not a
pause.** It is refused as `Error::Corrupt` with no journal position, and the
message names every section the body is short of, in the order the format names
them, in the same phrase `crate::task` refuses a task block with. It names only
those: a reader told all five were missing when one was has four false leads with
the message.

**The reading is one pass, the first label wins, and it is not fence-aware.** A
section runs from its label to the next, prose above the first label belongs to
none of them, and a label written twice keeps the text written under it first, as
a plan document's duplicate does (ADR-0007). A line that merely starts like a label
— `Impact of either choice:` — is prose, because the colon has to follow the word
directly. Fenced code is not skipped: a report quoting this format inside a fence
opens the sections it quotes. Fence-awareness means the scanner `crate::task`
built for a Markdown document, and it prevents the cheaper failure — a quoted
template read as a question — at the cost of the dearer one, an unclosed fence
that loses the question and everything written below it.

**One call asks for the record.** `decision_event` returns the `DecisionRaised`
entry to journal, or `None` for a report that claimed `DONE` or `FAILED` and so
asked for nothing, or the refusal — so no caller can take the event and skip the
check that produced it. `decision_request` stays public beneath it, because the
ask and the record are two facts: a screen listing questions should not have to
build an event to read one.

**The record moves the two states where an agent works, and nowhere else.**
`Running` and `Remediating` park themselves above their own attempt with
`PauseReason::Input`, so the answer resumes the attempt that asked with its
phases intact (ADR-0026). The six states with no agent at work have no report to
read a question out of and refuse it; `Paused` refuses it for the reason that same
ADR gives a second pause. ADR-0022's rule that each state owns one exhaustive
match is why the refusal is written eight times rather than once: the entry has to
be answered where every state says what it does with it. The two legal rows join
the 56 the transition sweep compares against the 264 pairs of states and entries
(ADR-0025). The payload is stored inline, as `docs/DESIGN.md` spells it and as
`AttemptRecorded` holds an `AttemptRecord` (ADR-0063): boxing changes the Rust
shape without changing one byte of the JSON the journal stamps (ADR-0016) or
`--json` prints, and no size lint asked for it.

## Alternatives considered

- **Treat any `NEEDS_INPUT` as a pause, question or not.** That is the shape the
  prompt leaves today, and it is unanswerable: `resolve` turning a blank into an
  ADR injects a blank into every later prompt. Refusing costs one retry and names
  the section to send back.
- **Read the sections out of a `DONE` report too.** A report that finished the
  work and mentions a question finished the work. Pausing on it is the false stop
  the asymmetry above rules out, so the header decides and the body never does.
- **Accept a near miss — `question:`, `Tradeoffs:`, `Recommend:`.** Each one can
  only be resolved by guessing which field the text feeds, and guessing reads one
  section's contents into another's answer. Strict labels buy a loud, fixable
  refusal; ADR-0076 made the same trade for the header.
- **Reuse the task block's section reader.** One parser for labelled text is
  tempting, but a report is not a plan: it has no headings, no fences worth
  skipping, and its required sections are four fields rather than a title and a
  verify line. Sharing the scanner would import Markdown rules into plain text.
- **Put the request in `TaskState::Paused`.** Every pause would carry a field
  most reasons never fill, the journal replay would have to rebuild it anyway,
  and ADR-0026's nested-pause encoding would have to grow around it.
- **`Box<DecisionRequest>` the payload.** Rejected with `AttemptRecorded` as the
  precedent (ADR-0063): the encoding is identical, the state machine, the journal
  and `--json` all read one inline shape, and a pointer is a Rust-only cost paid
  against a size problem this enum does not have.
- **Require `Recommended`.** It is the one section an agent at a real fork may
  honestly not have, and requiring it teaches reports to invent a preference a
  human was asked to form.

## Consequences

- `.ktask/prompt.md` prescribes no decision sections, so a report in the shape
  the runner asks for today is refused naming `Question:`. A test pins that
  refusal rather than working around it: the prompt is versioned configuration a
  task must not edit, so teaching agents the five labels is a decision-maker's
  change, recorded as a finding by this task.
- Until that change lands, a real `NEEDS_INPUT` cannot open a wait: the runner
  will refuse it as malformed and the attempt will be filed without one. The
  refusal is the visible half — it names the sections — and it is what makes the
  gap loud rather than silent.
- The question lives in the journal record, not in the state. A screen that shows
  a pause shows its reason; an inbox that shows the ask reads the `DecisionRaised`
  payload, which means `TaskState::Paused` cannot answer "what is being asked?"
  on its own. Whoever builds the inbox (T148) reads the journal.
- A report quoting these labels inside a fenced block has its quoted labels read,
  and the first copy wins. The fix, if a quoted template ever reaches an inbox,
  is the fence scanner `crate::task` already has — and the test that pins the
  current reading says which way the trade went.
- Nothing calls `decision_event` yet. T088's runner journals what it returns,
  T117's `resolve` answers it, and T148's inbox shows it. Until the first of
  those lands, `NEEDS_INPUT` is parsed and refused but still opens no wait.
- `DecisionResolved` stays deferred with the five other absent entries: the answer
  is an ADR, ADR-0078 already collects those into prompts, and the journal entry
  for an answer arrives with the task that records one.

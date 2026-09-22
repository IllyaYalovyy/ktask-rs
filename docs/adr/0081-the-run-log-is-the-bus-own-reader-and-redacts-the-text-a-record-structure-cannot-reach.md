# 0081. The run log is the bus's own reader, and redacts the text a record's structure cannot reach

- **Status:** accepted
- **Date:** 2026-09-21

## Context

T086 asks for "a run leaves a readable log, redacted", and the two documents
constrain it from opposite sides. VISION.md §11 makes the log a privacy surface
— "tokens, credentials, and configured secret patterns are redacted from logs",
"restrictive filesystem permissions" — while §13 makes it the data behind the
Logs screen: "raw and structured views, search, filters, follow mode, error
navigation", over output that is "always attributable to a task, a phase and a
moment in time". `docs/CONTRACT.md` gives that screen "filter by level and
phase". A log that satisfies §13 by writing prose, or §11 by writing nothing,
fails the other.

Five things had to be settled:

**Nobody must have to remember to log.** The task's own wording is "subscribe
the logger to the event bus so every event is logged without any caller
remembering to". The only place an event is emitted is
`Recorder::record`, which appends and only then
publishes (ADR-0016), so an event is durable before any log sees it: the log is
the *reading* of the record, not the record.

**Two redaction helpers already exist and they touch different halves.**
`redact::redact_json` rewrites a JSON *document*
(ADR-0032), and the journal uses it because a payload's own field names and
values are all inside one document. A log line is not one document to redact:
its six columns are the interface the screen filters on, and a text rewrite over
the finished bytes cannot tell `{"message":"…token…"}` from a secret that
happens to look like `"task_id":`.

**The bus loses events on purpose.** ADR-0030 settled the bounded ring, and
`Subscription::drain` reports what it dropped. A log that silently skipped
events would be a false record rather than a short one.

**Nothing in the catalog says how loud an entry is.** `EventKind` is a catalog
of facts, and `apply` is a pure projection over it (VISION.md §22's "decision
logic is pure"). A severity carried on the entry would be an opinion about
attention written into the durable format.

**The columns are not always known.** A `TaskQueued` row names no attempt and
no phase; an entry about the queue names no task. §13's attribution is still
 owed for a line whose own payload says nothing — a failure happened *in* a
phase even when the entry naming that phase was never written.

## Decision

**Six columns, always all six.** Every line is
`{"ts","level","task_id","attempt","phase","message"}`, written by `serde_json`
from a typed record, and a column the moment does not supply is `null`. A reader
that must handle two shapes for one format reads the wrong one eventually, and
`null` is the honest answer to "which task?" about an entry about the queue.

**`redact::redact` is applied to the message text, and `serde` owns the
structure.** Redaction still happens inside the write path, which is ADR-0032's
requirement; what changed is which half it can reach. Structure a secret pattern
cannot touch cannot be defeated by a secret, and a filter that reads a column
cannot be broken by a rewrite of the text beside it.

**The logger subscribes for itself and writes on its own thread.**
`Logger::subscribe` takes a `Subscription` on the caller's thread, so what the
log sees is what a frontend attaching at the same moment would see, then drains
it on a thread the `Logger` owns. A `flush` is answered by the writer, so what a
checkpoint asked for is on the storage before the call returns; an idle tick
writes and syncs nothing. `finish` (or the drop that follows) reaps the writer.
A writer that is no longer there is reported as `Error::Io` naming an
incomplete log rather than assumed to have finished, and a writer that died
panicking is reported rather than re-raised: a supervisor that unwinds over its
log loses the run it was supervising.

**`Level` is the log's vocabulary, mapped exhaustively over the catalog.**
`Debug` is the agent's output — the part of a run that is volume rather than
fact; `Info` is ordinary lifecycle; `Warn` is a run that stopped or lost
something without a check refusing; `Error` is a check that refused or a run
waiting on a human. `level_of` matches every entry, so a catalog entry a later
task adds has to be *placed* here rather than arrive at a level nobody chose. A
gate is placed by its own verdict, not its name.

**Filtering hides lines, never facts.** `AttemptStarted` and `PhaseEntered` are
read into a per-task standing *before* the threshold is applied, and each line's
`attempt`/`phase` is the one its payload names, or the one its task was last
seen at. A run logged at `Error` still attributes its failures to the phase that
produced them.

**A lost event is a line, not silence.** A drain that reports dropped events
writes one `Warn` line, `RingLost given=N`, and that line is not subject to the
threshold: a line about what the log cannot show is not one anybody opted out of
reading.

**One file per day, held like every other privacy surface.**
`<state_dir>/logs/run-<date>.jsonl` at `0600` inside a `logs` directory at
`0700`, the modes the journal, the prompt library and an attempt's evidence are
kept to. The mode is set on every open, not only asked for at creation, because
only a set takes a grant back. The state directory must already exist
(`Error::NotFound`), and a level of the layout that is a file or a link is
refused (`Error::Policy`) rather than written through.

## Alternatives considered

- **`redact_json` over the finished line.** One helper for both durable writes,
  and it would also redact the `ts` and column names — which is to say it can
  damage the format rather than only its text. It lost because here the half
  that can hold a secret is exactly the half that holds the text.
- **Recording the log inside `Recorder::record`.** It cannot be skipped, which
  is the point, but it puts a synchronous file write and a sync in the path
  that commits a journal row, and the log would then be able to fail a
  transition the journal has already accepted. The journal is the record; a
  slow reader must not be able to stop it.
- **A `level` (or `severity`) field on `EventKind`.** It would move the choice
  into the durable format, where a later change is a schema change, and it would
  make `apply` read an opinion it does not need. `GateFinished` is the entry that
  proves the point: its loudness is its verdict, which the catalog cannot say.
- **Level and phase as prose inside the message.** `CONTRACT.md`'s "filter by
  level and phase" then means parsing prose, and §13's screen re-parses it per
  line per keystroke.
- **Skipping the facts a hidden line carried.** Cheaper bookkeeping; it makes
  the threshold change what the log *means*, not only how much of it is shown.
- **Trusting the ring to be empty, or saying nothing about what it dropped.**
  The quiet option, and the one that makes a truncated log indistinguishable
  from a complete one.
- **A file per run, named by a uuid or rotated at midnight.** A run crossing
  midnight would answer "what happened in this run" with two files, and the
  screen would need a run→file index that nothing else maintains.
- **Creating the state directory, or following a `logs` symlink.** Both write a
  run's most sensitive artifact somewhere the registration did not choose
  (VISION.md §11's "no `.ktask/` in project repositories").
- **A `log(event)` call every frontend may make.** It is how the other tools do
  it, and it is the failure the task named: a caller that forgets leaves a run
  with no log.

## Consequences

- A power cut can cost the log its last few lines and the run loses nothing,
  because the journal already holds them. Anyone tempted to fix that should
  read this ADR first: making the log durable-before-effect would make it the
  journal.
- `RingLost` is not a catalog entry. It exists only in the log, so a screen
  reading the journal and a screen reading the log disagree by exactly that
  line — and the disagreement is the honest one.
- The TUI's Logs screen (VISION.md §13, a later task) reads this format and can
  filter on `level` and `phase` without parsing text; search and follow mode
  tail the same file. Retention is still §11's open question and is not decided
  here.
- Two loggers opened on one day append to one file, each writing whole records
  in one `write_all`, so a reader following the file never sees half a line.
- A `Logger` owns a thread for as long as it lives and `Drop` joins it, so a run
  that unwinds leaves a file rather than a dangling writer. A run that never
  drops or finishes its logger leaks one thread.
- No dependency was added: `serde_json`, `time` and `regex` were already
  required by the journal and the redaction table.

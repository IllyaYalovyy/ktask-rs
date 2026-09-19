# 0052. A scenario replays one step per session, in the order the file wrote them

- **Status:** accepted
- **Date:** 2026-09-19

## Context

ADR-0051 fixed what a scenario file may say. T056 has to decide what happens when
a session runs one of its steps: `Dummy` becomes a `Provider`, and the done-when
is two clauses — "the same scenario produces byte-identical output and journals
across runs", and "running out of steps is a clear error".

The format is deliberately quiet about both. What is actually at play:

- **A step names a session, and an `Invocation` does not.** A step's cue is
  `on_task` or `on_attempt` (ADR-0051), but the call a provider receives is a
  prompt, a model and a working directory — everything a CLI needs, and no
  identity for a cue to match against. Something still has to pick the step.
- **"Byte-identical" has to mean bytes someone can compare.** A replay is seen
  from three places at once: the files a session writes, what it answers, and
  what a watching frontend is shown. The third arrives as `Event`s, whose
  envelope carries a sequence and an instant (ADR-0016) that no two runs can
  share.
- **One line, or one blob.** `EventKind::AgentOutput` is defined as one line of
  agent output, and a live ring is sized in `output_ring_lines`. A step that
  printed forty lines as one record spends forty lines of budget to show one row.
- **`hang` is a response that does not arrive.** VISION.md §12 lists it beside
  the four responses that do, and §15 uses it for crash recovery: its whole point
  is that the watchdog, not the adapter, ends the session.
- **A scripted session is not measured.** It spent no tokens and disclosed no
  session id, and ADR-0049 already decided that an unreported figure stays
  unknown rather than becoming zero.
- **A step writes into a worktree that may not have the directory.** ADR-0051
  refused paths above the working directory and left the missing-parent question
  to the writer, on purpose.
- **One adapter serves a whole run**, including a review task configured for a
  different provider (ADR-0050), and every `Provider` method takes `&self`.

Measured, on rustc 1.98, against the definitions this ADR describes:

- Two runs of one three-step scenario, replayed through the document
  `Scenario::to_toml` writes, agree on every file byte, on every `Outcome`, and
  on the `serde_json` payload bytes of every published line — the test is
  `the_same_scenario_replays_byte_identically_across_two_runs`.
- A `hang` step hands its declared file to the filesystem and its declared line
  to a watching subscriber, then does not return: after 500 ms the invoking
  thread has produced neither an `Outcome` nor an error, and the file it wrote
  before it stopped answering is on disk (`a_hang_step_does_not_answer_and_its_
  watchdog_is_the_only_thing_that_ends_it`).
- A scenario of one step refuses a second session as `Error::Provider` with
  `dummy` and "session 2 has no step to replay: this scenario declares 1 step",
  writes nothing, and refuses a third ask identically.
- Two sessions started at once against one `Dummy` are handed two different
  steps, so the cursor is not a `&mut` the caller would have to serialize.

## Decision

**A step is chosen by position, one step per session.** `steps[0]` answers the
first session a `Dummy` is asked to run, `steps[1]` the second, from an
`AtomicUsize` cursor (`Relaxed`: what a caller is owed is a distinct index, and
the script it indexes was fixed before the adapter was built). The cue does the
two things a declaration can do — it lets `Scenario::validate` refuse a step that
answers nothing or answers two rules, and it attributes what the session printed.
Ordering steps against the queue belongs to the runner, and only the runner,
because only it knows which task and which attempt it is starting. A scenario
whose steps are out of queue order replays them out of order, in a file a
reviewer can read.

**A session does its steps in one order: wait, write, print, answer.** The
declared `delay_ms` first, then its files (parents created, below the invocation's
working directory), then its published lines, then the answer. Files before the
answer is what lets a `limit` or `needs_input` pause on a working tree that
exists, and lets a hung session leave behind state that recovery has something to
resolve. A missing parent is created rather than refused: a declared file in a
directory the run has not made is the ordinary case — the first session of a task
writes the first file of it.

**Running out of steps is `Error::Provider`, naming both numbers.** "session 2
has no step to replay: this scenario declares 1 step, and one step answers one
session". "The provider failed" would send an operator to the CLI when the file
is what is short. A refusal that cannot write a file is the same shape, naming
the step, the path, the directory and the OS reason.

**One `AgentOutput` per declared line.** A newline ends a line rather than
opening another, so `"a\n"` is one line and `"a\n\n"` is two, the second empty.
The `Outcome.stdout` the caller receives is the declared text whole and verbatim;
the per-line split exists for the live view and changes nothing else.

**The envelope carries the identity the step declared and no more.** `task_id` is
the step's `on_task`, or `None` where the cue named an attempt; `attempt` is the
step's `on_attempt`, or where the file named none, this adapter's own count of
the sessions it has run. Sequence is zero and the instant is the moment the line
was read: the journal assigns the real sequence and stamps its own instant as the
row is appended (ADR-0016), so **byte-identity is a claim about the payload**, the
files and the answer — never about the envelope, which is the journal's to write.

**`hang` withholds exactly one thing.** Everything the step declared happens, and
then the session never returns: `thread::sleep` in a loop with no term, so the
watchdog is the only thing that ends it. An `Outcome` here would delete the thing
the step was written to test.

**Capabilities are all `false`, and the answer is load-bearing.** A scenario
declares text, names no model, and measures nothing, so a capability the format
cannot honour is the mismatch VISION.md §12 says to reject rather than tolerate —
and a `false` is the only answer that lets a caller refuse instead of guess.
Where it refuses is preflight's and the factory's decision.

**A scripted session reports no figures.** `usage: None` and `session_id: None`,
per ADR-0049; `stderr` is empty because the format declares one stream and the
step's text is on it. `exit_code` is the step's declared or implied status — the
one figure a scenario is allowed to report, because it declared it.

**Validation happens in `Dummy::new`, not in `invoke`.** A `Dummy` that exists has
nothing in it that could fail to replay, which is what lets a session run a step
without re-checking it, and it means a scenario assembled in code is refused by
the same rules, in the same words, as one read from a file.

## Alternatives considered

- **Look the step up by cue.** Precise, and impossible: `Invocation` carries no
  task or attempt, so there is nothing to match a cue against. Extending the call
  with runner identity was rejected — it would make every adapter carry the queue
  to answer a question only the queue can ask, and ADR-0050's `Invocation` is the
  shape a CLI needs, not the shape a scheduler has.
- **A cue-keyed lookup with position as a fallback.** Two selection rules where
  one file declares one meaning, and a scenario cannot tell which one it is
  under. Refused.
- **A long finite sleep for `hang`.** Any term at all makes the adapter, not the
  watchdog, the thing that ends a hung session; a scenario testing that a hung
  attempt is recovered would instead be testing a timeout the dummy chose.
- **One `AgentOutput` for a step's whole output.** One publish instead of N, and
  it spends a whole ring to display a row, and a frontend shows a wall of text
  where the catalog promised a line.
- **Fixed `seq`/`ts` in every published event.** It would make the envelope
  byte-identical too, by shipping a timestamp no run believes. The journal's row
  is the record that has to be reproducible; a producer guessing at its columns is
  what ADR-0016 exists to prevent.
- **Refuse a file whose parent directory is missing.** One refusal that a
  scenario's first step hits every time, and it would push "mkdir" into every
  fixture for no gain in safety — the path was already refused above the working
  directory at load time.
- **Echo the declared text into `stderr` as well.** Two streams of one lie: an
  operator reading a failure would see the same text twice and the classifier
  would be choosing which one to trust.

## Consequences

- A scenario has to declare at least as many steps as the run has sessions, and
  when it does not, the refusal says so in numbers. Scenarios will be longer than
  the number of interesting moments in them; that is the cost of position order.
- The live view's attribution is provisional where a cue named no attempt: it
  carries this adapter's session count, not the runner's attempt number. The
  journalled record is the one that attributes truly, which is what
  VISION.md §3's "nothing is done on an agent's say-so" already assumes.
- `hang` is untimed, so anything that invokes a `hang` step must do it from a
  thread it can abandon and must never join. The test does exactly that, and any
  future scenario runner has to as well.
- A scenario cannot declare `stderr`: a failure's text can only be classified
  from `stdout`. Adding a `stderr` key is a format change (ADR-0051's rules
  apply), and this task deliberately did not add one.
- Nothing here chooses a scenario. `Config::dummy_scenario_path` has no default,
  and `Dummy::load` refuses a path holding nothing; wiring the setting to a
  provider instance is the factory's, not this adapter's.

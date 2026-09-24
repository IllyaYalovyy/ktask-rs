# 0094. A command's answer is named, and the number it becomes belongs to the CLI alone

- **Status:** accepted
- **Date:** 2026-09-23

## Context

`docs/CONTRACT.md` §1 promises seven answers and pairs each with a number: drained
0, a task failed 1, usage error 2, provider limit 3, human gate 4, needs input 5,
interrupted 130. It also draws the line that matters — "Codes 3, 4 and 5 are
pauses. They are not failures and must never mark a task `failed`."

Until now nothing in the workspace could hold that promise, because nothing had
an answer to give. `Runner::run_task` returns `Result<TaskState>`: a pause arrives
as `Ok(TaskState::Paused { .. })` — ADR-0093 made that deliberately, refusing to
spell a pause as an [`Error`] — and a caller that has to produce a status must
open-match on a state enum whose eleven variants say where a task *is*, which is
not the same question as what the run should *report*. ADR-0093 said so in its own
words: "The `RunOutcome` the CLI will map to exit codes belongs to the task that
owns that enum."

Three forces made this a decision rather than a transcription of
`docs/DESIGN.md`'s definition.

**Two frontends, one answer.** VISION.md §1 puts the TUI first and §5's second
paragraph makes both frontends thin over this crate. A number is only readable by
a process that exits. A TUI showing a paused queue has nothing to show if the
core's answer is `3`, and every attempt to help it — a `match` in the frontend
that guesses which pause produced the number — is behavior in a frontend, which
CONTRACT §0 rule 2 forbids.

**A pause must not be reachable from the failure path.** §1's rule and §3's
invariants 5 and 8 both say a limit, a gate and a question are not failures. Any
representation where one value can be both, or where a pause is a failure carrying
a note, makes the rule a runtime convention; §3's whole method is that an
invariant an agent can ignore is not one.

**Where the numbers are allowed to be said.** The mapping is the CLI's job
(T105), and prose in this crate that recites the table quietly re-puts the table
where the type took it out of. Several doc comments written before this type
existed did exactly that, including one in `classify.rs` that explained a missed
limit pattern by what it reports at the top level.

## Decision

**`RunOutcome` is the enum `docs/DESIGN.md` *Other fixed types* fixes, in
`runner.rs`, re-exported from `lib.rs`, with no number in it.** Seven variants,
one per documented answer: `Drained`, `TaskFailed { task }`, `Usage { detail }`,
`ProviderLimit { until }`, `HumanGate { task }`, `NeedsInput { task }`,
`Interrupted`. It derives `Debug`, `Clone`, `PartialEq`, `Eq` and nothing else —
not `Serialize`, because `--json` output is the CLI's shape to fix (T106), and not
a `Display` that prints a status word, because the word an interface chooses is
that interface's.

**No function in this crate produces or consumes an exit number.** There is no
`exit_code()` method and no `From<RunOutcome> for ExitCode` here; T105 writes
`exit::code_for` in `ktask-cli`, and that is the one place 0, 1, 2, 3, 4, 5 and
130 appear as literals. The doc comments in this crate that used to name those
numbers now name the variants that carry the same meaning, in `runner.rs` and in
`classify.rs` alike. The exit codes a *subprocess* left are a different fact and
stay where they were: `GateResult`, `AttemptFinished` and the provider adapters
record them by name because VISION.md §3's fourth invariant is a rule about not
trusting them, which requires keeping them.

**An outcome names the task it stopped at, not the reason it stopped.**
`TaskFailed`, `HumanGate` and `NeedsInput` each carry a `TaskId` and no
`FailureClass`, no pause reason and no detail. Who and where is what a caller acts
on — `retry --task`, `ack`, `resolve --task`; why is already in the journal, and a
second copy in the answer is a second copy that can disagree.

**The three pauses are three variants, and `Interrupted` is not one of them.**
`Interrupted` is the run's own end rather than a stop the queue reached: §1 gives
it its own number precisely because it is neither a completion nor a refusal, and
T103's signal handling is what will produce it.

## Alternatives considered

- **`impl RunOutcome { fn exit_code(self) -> i32 }` in the core, with the CLI
  calling it.** One fewer place for the table to drift, and it loses for the same
  reason `Provider::exit_status` would: the core then knows it is being run as a
  process, the TUI has an answer with a number welded to it, and the `--json`
  surface gains a field nobody asked for. T105's table-driven test is where the
  table is checked, and one test is enough.
- **A `detail: String` on every variant.** `ProviderLimit` and `Usage` carry what
  an operator needs because their pause *is* that information — when the ceiling
  lifts, what was wrong. On the other three it would be a string a frontend prints
  and a second account of a journal row.
- **`RunOutcome` in a module of its own.** `docs/DESIGN.md`'s module list has no
  file for it and the task named `runner.rs`; it belongs with the loop that
  produces it. It can be moved out by the task that finds the module list wants
  it elsewhere, which is a rename, not a redesign.
- **Deriving `Serialize` now, so `run --json` can print it.** `--json` is a
  compatibility surface (CONTRACT §0 rule 3) and its shape belongs to T106, which
  will decide whether an outcome is serialized as a tag, a tag plus fields, or a
  rendered line. Deriving it here would fix that format with no test behind it.
- **Reusing `TaskState` as the answer.** Eleven variants of "where this task is"
  against seven answers, three of which (`paused` with three different reasons)
  collapse and none of which distinguishes a usage error, which is not a state at
  all. The projection and the report are two questions.

## Consequences

- The enum is inert until something returns it, and that is exactly the split the
  queue already made: T099 writes `run_queue` and it returns `RunOutcome`; T105
  maps it; T107 dispatches every `cmd::` function through it. This task defines
  the vocabulary, so those tasks have one word each for an answer instead of
  inventing one at the call site — and until T099 lands, no caller exists, which
  the tests below state as properties of the type rather than as behavior of a
  run.
- `Usage` lives in the core enum although nothing in the core can produce it — bad
  arguments are parsed in the CLI. `docs/DESIGN.md` fixes the enum with it, and it
  is right to: the CLI's dispatch has one return type for every command, and a
  parse refusal answered in some other channel would be a second way to report.
- A pause has one spelling. A run that reaches a limit, a gate or a question
  answers with a variant whose name is not `TaskFailed`, so §1's "must never mark
  a task failed" is now a type distinction the mapping task cannot get wrong
  silently; `exit::code_for` still has to map each to its own number, and T124
  asserts the three end to end.
- The tests in `runner::outcome` pin the *documented* set: the total match in the
  test module's `meaning` helper fails to compile if an eighth variant arrives, and
  its distinctness assertion fails if two documented outcomes ever share a variant.
  What those tests cannot pin is the number, by design — that is T105's table.
- `Interrupted` has no producer yet, and nothing in the core sets it. T103 wires
  the signal; until then a run that is killed reports nothing, which is the honest
  state of a queue that has not reached that task.

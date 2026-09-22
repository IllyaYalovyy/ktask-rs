# 0088. A phase is decided by the difference between two gate runs, and a refusal files what it ran

- **Status:** accepted
- **Date:** 2026-09-22

## Context

VISION.md §9 hands the red and green phases to the runner rather than to the
agent's account of itself: *"the runner executes `targeted_test_command` and
confirms the expected new failure"*, and then the same for the fix that ends it.
The two rules are already written and tested in [`crate::protocol`] as
[`crate::protocol::verify_red`] and [`crate::protocol::verify_green`]; what is
new here is running a command and deciding a phase from what it printed. Six
forces made that a decision rather than a call.

**The exit status answers the wrong question.** A red phase is *expected* to
exit nonzero — a red phase that passed its new test proved nothing — so a status
cannot decide it. The other direction fails too: a `cargo test` that printed
counts and then died, or a workspace whose second test binary failed to build,
names tests that all pass and still exits 1. And [`crate::parse_cargo`] answers
[`None`] for output with no `test result:` line, which is a run that reported
nothing and not a run with zero failures.

**A phase that decides itself by comparing two runs can be handed one summary.**
The cheap answer is a default: an empty "before" makes every newly-failing name
new, which passes a red phase that broke the entire suite. That default is what
invariant 4 of §3 exists to refuse, so the step has to say what it was not given.

**Where the refusal goes matters more than that it exists.** §3's invariant 3
wants every transition journalled before its side effect, and ADR-0036 fixed how
a lone [`crate::EventKind::GateStarted`] reads: a gate that never completed. A
step that journalled a start before noticing it had nothing to compare would
leave the run holding the account of a gate whose answer nobody could read.

**§9's exception to test-first is a gate that does not run.** Documentation,
pure refactoring, build configuration and a bug already covered by a failing
test skip red. Not running a check is precisely the thing the rest of this crate
refuses to accept on request, so the claim needs something to be checked
against.

**A refused phase still has to be readable.** §9 stores RED and GREEN evidence —
command, output, tree hash — with the attempt, and ADR-0065 gave an attempt a
file home for what a row cannot carry. But [`crate::write_evidence`] refuses a
second, different record for one attempt, which is right for the record and wrong
for a red phase that was refused and ran again. ADR-0065 also recorded that the
attempt's other files were written unredacted; gate output is the one artifact
whose whole purpose is printing what a test said verbatim.

**The task's one-line signature omits what the work needs.** `GateStarted` and
`GateFinished` are rows belonging to a task, §9's exception is read from the
task's declaration, and evidence has an attempt's directory. None of those is in
`(prep, spec, before)`.

## Decision

**The method takes the task and the attempt as well**: `gate_phase(&mut self,
prep, task, attempt, spec, before) -> Result<TestSummary>`. The two additions are
the parameters the task body's own requirements ask for — a row's owner, the
declaration an exception is read from, the directory evidence is filed in — and
`TestSummary` comes back because it is the next phase's `before`: red's after
is green's before, and a step that returned `()` would make the caller re-run the
gate to find out what it just ran.

**`Comparison` has three arms, and the refusal comes first.** Red and green
carry the summary they must differ from; every other phase a protocol holds is
decided by one run's verdict and carries nothing, which is what keeps a refactor
handed no comparison from being refused for a rule no declaration wrote.
[`comparison`] runs before the first row and before the first spawn: a phase
asked a question with both halves missing is the step called wrongly, not a phase
that failed.

**The verdict is read from the two summaries, and the gate keeps the last word.**
Red is [`crate::protocol::verify_red`] alone, because red is *supposed* to refuse.
Green is [`crate::protocol::verify_green`] against every name the previous run
left failing, then the gate's own verdict — so a phase that fixed its test and
broke another is refused by the rule that names the test, and a phase whose names
all passed on a command that refused is refused by the run. The order is the
actionable one: "green left `tests::a_previous_refusal` failing" tells an
operator what to fix, "the command exited 1" does not. Green returns the names it
was for, not the names that happen to have passed.

**Evidence is filed whatever the verdict said, and the verdict is what returns.**
Both are computed, then `verdict?; filed?;`. A refusal nobody can read is a
refusal nobody can act on, so a red phase that proved nothing files the command,
the output and an empty `names` before it is refused.

**A phase's evidence is one appended line in `phases/<phase>.jsonl`,** owner-only
(0600 in a 0700 directory), fsynced, and redacted with the project's own
`secret_patterns` through [`crate::redact::redact_json`] before it reaches disk.
It holds §9's three things — command as the words it was spawned with, both
streams verbatim, and `tree_sha` — beside the phase, the gate, the base and the
gate's stored verdict. `tree_sha` is a SHA-256 over a domain prefix, the base sha
and the content digest of every path `changed_paths` reports, because the tree
the gate ran against is the working tree and git has no OID for a set of unstaged
edits; `HEAD` names the base, which never moves here.

**The exception is checked before it excuses anything.** [`crate::protocol::claim`]
weighs the task's declared category against the phase's own write scope and the
profile's test globs: a documentation exception over a production path leaves the
same [`Error::Policy`] naming that path as refuses the phase, with no gate row
written; a claim the scope supports records
[`crate::EventKind::TddExceptionUsed`] and hands on the summary the phase started
from, so green is still compared against a real measurement. No gate pair is
invented for a gate that did not run — an invented pair reads as a check that
passed.

**Gate rows are filed under `Some(task.id)`,** where the completion set and the
preflight file theirs under `None`. A failures screen opens a task's rows, and a
phase whose gate appears in none of them is indistinguishable from a phase that
ran none.

**The step moves no state.** No `apply`, no failure record, no retry: the pair of
rows and the returned summary are the whole output, exactly as ADR-0086 and
ADR-0087 decided for a session's end and a phase's answer. Mapping a verdict onto
a transition is the queue walk's job.

## Alternatives considered

- **Decide red and green from the exit status.** One code path, and it is wrong
  twice: red must fail, and a green run can report passing names while the
  command refuses. The status is kept in the evidence, where it can be read.
- **`TestSummary::default()` when `before` is `None`.** Silently the most
  permissive possible baseline. Refused for the same reason §9 exists.
- **Refuse the missing comparison after the gate ran.** One fewer early return,
  and the loss is §3's invariant 3: a `GateStarted` row whose verdict the caller
  could never have read, which ADR-0036 says reads as an incomplete gate.
- **`write_evidence` for the phase record.** It would refuse a phase's second
  run, so the second-round file would have to be named for the attempt or the
  round, and reading one phase's history would become a glob. An append-only
  JSONL says both runs happened, in order, in one file named for the phase.
- **`red-1.jsonl`, `red-2.jsonl`, …** Rejected for the same reason: the count is
  unbounded, the caller would own it, and nothing downstream reads a count.
- **File evidence only when the phase passed.** Saves a line nobody asked for and
  loses the case that matters — a refusal whose output is the diagnosis.
- **Redact at read time, or not at all.** Read-time redaction cannot be verified
  by the test that reads the file, and ADR-0065 already showed what an
  unredacted artifact does: a secret that only ever appeared in evidence still
  reached the tree it was filed in.
- **Put the red/green rules in `runner.rs`.** They are written and tested in
  `protocol.rs`; a second copy is a second place to be wrong.
- **A `PhaseSpec::exempt_to_tests` flag** instead of reading §9's category from
  the task. It would skip red without naming a category §9 knows, which is what
  makes the exception auditable in task history.
- **`apply` the phase's transition here.** ADR-0086 settled the same question for
  a session's end: the row is not the transition, and the queue walk owns it.

## Consequences

- A red phase that broke nothing, and a green phase that fixed its test and broke
  another, are now mechanically refused and name the test each one is about. That
  is the whole point of §9, and it holds against a report that claimed otherwise
  because the gate is run here, in this process, against the checkout.
- `gate_phase` has no caller yet. T093's completion and T094's `run_task` own
  the walk that calls it and maps its `Err` onto a failure record; until then a
  refused phase is a value the run holds and does not act on.
- `phases/<phase>.jsonl` is a history, so "the red evidence" is the last line of
  a file rather than the file itself. A reader that wants the phase that finally
  passed reads the file; nothing in the crate does that yet, and the TUI
  inspector that shows a phase will have to.
- Two checkouts of one base whose changes are byte-identical hash the same, and
  ignored files are not in the digest. That is the question §9 asks — *is this
  the work that was gated* — and it means a phase that changed nothing hashes
  exactly what its base did.
- `config.secret_patterns` now reaches phase evidence. It still does not reach the
  journal: `Runner::new` never calls [`crate::Journal::with_secret_patterns`],
  the finding ADR-0087 recorded, and this step did not widen the runner's
  construction to fix it.
- Green compares against every name the prior run left failing, not only the one
  red added. That is stricter than §9's step 4 reads, and deliberately: a phase
  that fixed its new test and regressed an old one is not green, and the done-when
  for this task says so.

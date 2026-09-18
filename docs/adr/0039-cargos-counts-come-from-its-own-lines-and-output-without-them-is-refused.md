# 0039. Cargo's counts come from its own lines, and output without them is refused

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T043 asks for `parse_cargo(output: &str) -> Option<TestSummary>` in
`crates/ktask-core/src/gate.rs`. The signature is given; the decision hiding
inside it is when to answer `None`. VISION.md:168 asks that "common test output
formats (Cargo, JUnit, pytest, Flutter) are parsed into structured results while
raw output is retained", and the caller this feeds is a `verify` gate whose
result decides whether a task may be published — so the failure mode worth
designing against is not a crash, it is a *wrong summary that reads as a green
suite*.

Everything below was measured on this machine (cargo 1.98.0, libtest,
cargo-nextest 0.9.145) over scratch crates, and each fixture is committed
verbatim in the test module rather than paraphrased, because a parser written
against remembered output parses what somebody remembered.

1. **A run answers more than once.** A plain `cargo test` over a crate holding
   unit tests and one doctest writes two `test result:` lines — one per test
   binary. Neither the first nor the last line is the run: the first misses the
   doctest, the last reports a green library binary over a run whose doctest
   refused.
2. **`failures:` is written twice per failing binary** — once as headings around
   each test's captured output, once as the summary's own indented list of names.
   The first is followed by a blank line, which is what tells the two apart.
3. **`--nocapture` deletes the detail blocks entirely.** The names survive only in
   the second block, so a parser that read `---- name stdout ----` as the list of
   failures returns an empty list for a gate configured with that flag.
4. **`-q` collapses the status lines** to progress dots and lists the same
   failures in a different order; the headers, the name block and the count line
   are unchanged.
5. **`cargo nextest` quotes libtest back at four spaces of indent.** Its echo of a
   failing test contains a line reading exactly
   `test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 4 filtered out; finished in 0.00s`,
   while its own verdict is `Summary [   0.014s] 4 tests run: 1 passed, 3 failed,
   1 skipped`. A parser that searches for `test result:` anywhere in a line reads
   a run of 4 tests as a run of 1.
6. **A crate that never compiles writes 1164 bytes of rustc to stderr and nothing
   to stdout.** There is no count in it. A parser that defaults to zeros returns
   `0 passed; 0 failed` — the answer that lets a broken build be published.
7. **A crate with no tests still answers:** `running 0 tests` followed by
   `test result: ok. 0 passed; ...`, twice, once per binary. That is a summary of
   zeros, and it is a different fact from "this is not cargo's output".
8. **A run cut short stops mid-transcript.** Killed by its gate timeout
   (ADR-0037/ADR-0038 own the killing), a run leaves the tests that had already
   printed `... FAILED` and no count for the binary that was running.

## Decision

`Some` is answered only over lines that are cargo's own, at column zero, and only
when they are internally consistent. Five rules, each held by a test over a real
fixture:

- **The counts come from `test result:` lines and nowhere else**, summed over the
  run's binaries. Status lines (`test name ... ok`) are progress, and a
  `running N tests` header is counted only to notice a binary that never answered.
- **No such line, no answer.** A compile refusal, an empty transcript, a killed
  run that never reached a count, and another tool's output all return `None`.
- **Column zero, not trimmed.** nextest's quotation is indented and is therefore
  not a count; neither is a test's own stdout, which libtest re-emits unindented
  but never as a line that begins the report.
- **The three counts are read by their labels**, in order, and a number arriving
  under the wrong label refuses the line. Fields cargo added after them
  (`measured`, `filtered out`, the duration) are unread, so a line that grows
  another field still parses.
- **A run has to add up.** A verdict of `ok` beside a non-zero failure count (or
  the reverse), a `failures:` list whose length differs from the count beside it,
  and a `running N tests` header whose binary never wrote its count are each
  refused. The name list is what `GateKind::Targeted` re-runs and what
  remediation is aimed at, so a list of the wrong length is not a usable partial
  answer.

`TestSummary` carries counts and names, and **no verdict**. Whether a suite is
green is a decision over counts *and* over the run that produced them, and the
facts that tell a green suite from a killed gate (`GateResult::timed_out`, the
exit status) belong to the code that ran it, not to the code reading its
transcript. `Option` rather than `crate::Error`: no variant of `Error` describes
"this text is not in this format", and inventing one for a value that is simply
not a test run trades a returned value for a new durable error kind.

## Alternatives considered

- **Return `Some` with zeros for unrecognised output.** Rejected: it is the
  Done-when's named failure, and in this codebase it is the worst possible
  wrongness — a suite that never compiled reported as one that never failed.
- **Trim each line before matching, so indentation does not matter.** Rejected by
  measurement 5: it makes nextest output parse into a confident wrong answer
  (1 test, 1 failed) instead of a refusal.
- **Collect failure names from the `---- name stdout ----` headings.** Rejected by
  measurement 3: the headings are cargo's presentation of a failing test's
  output, and they disappear under `--nocapture` while the verdict survives.
- **Trust the count when the name list disagrees, and report the count.**
  Rejected: the caller acts on the names, and a test that prints `failures:` and
  indented lines of its own is otherwise indistinguishable from the real block.
  Refusing costs a summary that can be read by eye in the retained raw output;
  guessing costs a wrong list of tests sent to be fixed.
- **Summarise the binaries that did answer when one was cut short.** Rejected: it
  reports the fraction that happened to finish as the whole run, and the case is
  the ordinary one for a timed-out gate, which is precisely where a supervisor
  must not be optimistic.
- **Parse `nextest`'s `Summary` line here too, returning whichever format was
  found.** Rejected: the task names cargo, and one function that answers for two
  formats makes it impossible to tell a caller which tool it believes. A nextest
  gate gets its own reader, which the column-zero rule keeps from being confused
  with this one.
- **Return the raw output alongside the summary** so a `None` still carries
  something. Rejected: the raw bytes are already retained whole in `GateResult`
  (VISION.md §8 requires that), and a second copy in the return type would be a
  second owner of the same text.

## Consequences

Reading a gate's transcript is now a decision with a named refusal, so the code
that runs a `verify` gate has three outcomes to handle — green, red, unread —
rather than two, and unread must not be allowed to fall through as green. Nothing
in this task wires that up; `parse_cargo` is a pure function with no caller yet.

The strictness has a price that is accepted knowingly: output carrying ANSI
colour, output a test polluted with its own `failures:` block or its own
`running N tests` line, and a truncated excerpt of a run are each refused rather
than read. A gate runs with its pipes captured, where cargo writes no colour, so
the realistic exposure is the noisy-test case, and the raw output is retained for
exactly the moment it bites.

JUnit, pytest and Flutter are still unread (VISION.md §8 names all four); each
needs its own function and its own refusal rules, and the tests here are the
pattern for writing them. If a summary ever becomes journal data, its shape is a
durable decision — ADR-0036 is the precedent for pricing one.

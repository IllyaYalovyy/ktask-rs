# 0018. ADR recording moves to core so the interface and `resolve` share it

- **Status:** accepted
- **Date:** 2026-09-23

## Context

`docs/CONTRACT.md` §4 says the input inbox writes "the same ADR as `resolve`",
and T148 requires a test that the two are byte-identical for the same input.
The ADR's title, slug, numbering, rendering, redaction and write, and the
journal-first ordering of ADR 0009, all lived in `ktask-cli`'s `resolve.rs`.
`ktask-tui` cannot depend on `ktask-cli` (the dependency runs the other way),
so the interface could only reproduce that code, and two copies of a file
format drift.

## Decision

Move the recording into `ktask-core::adr`. `resolve_decision(project, task,
request, answer, today)` derives the task's state from the journal, numbers
the record, checks the transition, journals `DecisionResolved`, then writes
the ADR, and returns the path or a `ResolveError` naming the step that failed
(and so whether the answer is already journaled). `raised_request` and
`bullets` move with it. `ktask-rs resolve` keeps the parts that are its own:
usage checks before opening `$EDITOR`, the editor template, and the exit
codes. Its messages are unchanged. The interface calls the same function.

## Alternatives considered

- **Duplicate the code in `ktask-tui`.** Byte-identity would rest on two
  copies staying in step, tested by comparing them with each other.
- **Have the interface run the `ktask-rs resolve` binary.** It would need the
  binary on `PATH` and would make a headless test spawn processes; the
  supervisor's own state would be reached through a subprocess.
- **Move only the rendering to core, leaving the sequence in each caller.**
  The ordering that matters (journal before file) would again exist twice.

## Consequences

Core gains a small module; the unit tests for title, slug and numbering moved
with it, and the CLI keeps its end-to-end tests of `resolve`. `resolve`
derives the task's state a second time inside `resolve_decision` (once in the
CLI's own check before the editor opens), one extra journal read per answer.

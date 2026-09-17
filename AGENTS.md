# Working rules

Read this first, then `VISION.md`, then `docs/CONTRACT.md`, then the task
you were given. You are
working unattended: no one will answer a question mid-task.

## The loop for every task

1. Confirm the project is green before you touch it: `scripts/quality.sh`.
   If it is already red, that is a finding — report it, do not paper over it.
2. Do the work described by the task, and only that work. Scope creep is the
   top-ranked risk in VISION.md §16.
3. Run `scripts/quality.sh` until every gate passes.
4. Commit. Small, self-contained, message says what changed and why.
5. Report using the `KTASK_RESULT:` header (see below).

## Non-negotiables

- `scripts/quality.sh` is the definition of done — nine gates, all of which
  fail the build on violation. See `docs/QUALITY.md`. If a gate cannot run because a tool is
  absent, `scripts/check-prereqs.sh` names it and the command that installs
  it — that is a finding to report, not something to work around.
- Never weaken a gate to make it pass: no `#[allow]` on the line that
  triggered a lint, no `#[ignore]` on a failing test, no deleted assertion, no
  loosened threshold in `clippy.toml`. Fix the code instead. Weakening a check
  is a task failure even if everything then goes green. A lint may be
  suppressed only at an architectural boundary, at module or crate level, with
  a comment saying why the rule does not apply there.
- Test-first for behavior-bearing code. Write the failing test, see it fail,
  then make it pass. Exceptions: documentation, pure refactoring with existing
  coverage, build configuration.
- Tests assert behavior, not execution. Coverage only proves a line ran. A test
  that passes against a broken implementation is worse than no test, because it
  buys false confidence — and `scripts/review-tests.sh` is run over the
  finished work to count exactly how many of those were written.
- `.ktask/config.toml`, `.ktask/prompt.md` and `.ktask/context.md` are
  versioned configuration and must not be edited — they are how every run is
  made identical. `.ktask/queue/` and `.ktask/logs/` are per-run state and are
  never committed.
- Adding a dependency changes `Cargo.lock`, and the build gate runs with
  `--locked`. Commit the updated lock file in the same commit, or the build
  fails for the next task.
- Record significant design decisions as ADRs in `docs/adr/` (template:
  `0000-template.md`). Adding a dependency is a design decision.

## Reporting

End your run with a report whose first line is exactly one of:

```
KTASK_RESULT: DONE
KTASK_RESULT: FAILED
KTASK_RESULT: NEEDS_INPUT
```

Then state what you did, what you verified, and anything you deliberately left
undone. `NEEDS_INPUT` is for genuine ambiguity a decision-maker must resolve —
not for work you found hard.

If you cannot complete the task, say so and stop. Report `FAILED` with what you
tried and what blocked you. Do not weaken a gate, claim a success the gates do
not support, or keep retrying an approach that is not working — a recorded
failure is a usable result, while a fabricated success is not, and thrashing
spends the budget that later tasks need.

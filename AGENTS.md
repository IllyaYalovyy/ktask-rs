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
4. Run `scripts/review-tests.sh`. It mutates the lines you changed and checks
   that your tests notice. A surviving mutant means your code was made wrong
   and every test still passed. Add an assertion that catches it, delete the
   code it mutated, or state in your report why it is unreachable. Never
   weaken a test to get past it.
5. Commit. Small, self-contained, message says what changed and why.
6. Report using the `KTASK_RESULT:` header (see below).

## Non-negotiables

- `scripts/quality.sh` is the definition of done — eight gates, all of which
  fail the build on violation. See `docs/QUALITY.md`. Install what they need
  with `scripts/setup.sh`; `scripts/setup.sh --check` verifies versions.
- Never weaken a gate to make it pass: no `#[allow]` on the line that
  triggered a lint, no `#[ignore]` on a failing test, no deleted assertion, no
  loosened threshold in `clippy.toml`. Fix the code instead. Weakening a check
  is a task failure even if everything then goes green. A lint may be
  suppressed only at an architectural boundary, at module or crate level, with
  a comment saying why the rule does not apply there.
- Test-first for behavior-bearing code. Write the failing test, see it fail,
  then make it pass. Exceptions: documentation, pure refactoring with existing
  coverage, build configuration.
- Tests assert behavior, not execution. Coverage only proves a line ran;
  `scripts/review-tests.sh` proves your tests would fail if that line were
  wrong, and it runs on every task. A test that passes against a broken
  implementation is worse than no test, because it buys false confidence.
- `.ktask/config.toml`, `.ktask/prompt.md` and `.ktask/context.md` are
  versioned configuration and must not be edited — they are how every run is
  made identical. `.ktask/queue/` and `.ktask/logs/` are per-run state and are
  never committed.
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

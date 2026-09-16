# Working rules

Read this first, then `VISION.md`, then the task you were given. You are
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

- `scripts/quality.sh` is the definition of done. Never weaken a gate to make
  it pass: no `#[allow]` to silence clippy, no `#[ignore]` on a failing test,
  no deleting an assertion. Fix the code instead. Weakening a check is a task
  failure even if everything then goes green.
- Test-first for behavior-bearing code. Write the failing test, see it fail,
  then make it pass. Exceptions: documentation, pure refactoring with existing
  coverage, build configuration.
- Tests assert behavior, not execution. Test-suite quality is scored by
  mutation testing; a test that passes against a broken implementation is
  worse than no test.
- Never commit anything under `.ktask/`. It is operational state, excluded
  from the repository by construction.
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

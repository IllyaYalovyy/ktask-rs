# 0010. Recovery marks a dead attempt with no worktree interrupted

- **Status:** accepted
- **Date:** 2026-09-23

## Context

`reconcile` (`ktask-core/src/recovery.rs`) originally returned
`Error::Corrupt` for a task that was mid attempt, whose process was dead and
whose worktree was gone, on the reasoning that `run_task` removes its worktree
only after the attempt returns, so the combination "should never happen".

T118 runs `reconcile` at the start of `run` and `resume`. The scenario suite
showed the combination does happen: an attempt that ends in an error the
runner cannot journal (a provider that fails outright) leaves the task in
`Running` while `run_task` still removes the worktree on the way out. With
`Corrupt` as the answer, every later `run` and `resume` failed at recovery, so
one such task made the whole queue unusable, including `status`-driven
diagnosis of it.

## Decision

A dead process with no surviving worktree reconciles to
`Recovery::MarkInterrupted`, with a detail saying there is nothing to resume in.
The task becomes `Paused` on `Interrupted`, the decision is journaled, and a
person decides what to do next. Nothing is guessed and nothing is re-run.

## Alternatives considered

- **Keep `Corrupt` and let the CLI report it.** Recovery would abort before
  reconciling any other task, and the queue could never start again. Rejected:
  recovery must always leave the queue in a known state.
- **Make the runner journal `TaskFailed` for every attempt error.** This is the
  root cause, but it changes what `run` reports for those failures (exit 1
  rather than 130) and is a runner change well outside T118. Left as follow-up
  work; this decision remains correct after it, since a crash can also strike
  between the worktree's removal and the next journaled event.

## Consequences

`Corrupt` from recovery now means only that the journal itself is
inconsistent (a missing `AttemptStarted` or `PublishStarted`). A task left
`Paused` on `Interrupted` is reconciled again on each start until something
moves it on.

# 0013. `run` journals a failed attempt as `TaskFailed`

- **Status:** accepted
- **Date:** 2026-09-23

## Context

ADR 0008 recorded a known gap: when `run_queue` saw a task's attempt end in an
error — the agent wrote no report, remediation was exhausted with the
completion gates still red — it reported `RunOutcome::TaskFailed` but never
journaled `TaskFailed`. The task stayed `Running` or `Remediating`, so it was
not `Failed`, `retry` refused it, and a second `run` reported an unfinished
attempt (exit 130) rather than the failure. VISION.md §6 makes `failed` a
state and §7 makes `retry` the way out of it; T123 asserts that a remediation
that fails twice leaves the task `Failed` and the queue stopped, which the
journal could not say.

## Decision

`Runner::run_task` passes an attempt's error through `journal_failure` before
returning it. If the task is mid-attempt (`Running`, `Remediating`,
`Verifying` or `Publishing`) it records the failure through the same
`record_failure` a failed `retry` uses — classifying the error as the retry
path does, and journaling the `VerifyFailed` that `Verifying` needs first.
The error itself is returned unchanged. A task that is paused, interrupted or
cancelled already journaled its own verdict and is left alone.

## Consequences

A failed `run` and a failed `retry` now leave the same journal shape, and
`retry` works after either. A second `run` reports the failed task (exit 1)
instead of an unfinished attempt (130). Tests that asserted the old,
stranded shape now assert `TaskFailed` and the `Failed` state.

Recovery is unchanged: a process killed mid-attempt never reaches
`journal_failure` and is still reconciled as before.

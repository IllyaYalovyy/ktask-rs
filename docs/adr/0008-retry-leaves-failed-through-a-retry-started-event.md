# 0008. Retry leaves `Failed` through a `RetryStarted` event

- **Status:** accepted
- **Date:** 2026-09-23

## Context

`ktask-rs retry --task <id>` must start a fresh remediation attempt for a
failed task, "seeded with the failure bundle", and record an event rather
than edit anything (`docs/CONTRACT.md` section 3, T116). Until now `Failed`
was terminal in the strictest sense: `state::apply` rejected every event
against it, and no other state has an edge into it that a human can trigger.
Nothing in the catalog could carry a retry. `Resumed` leaves only `Paused`,
and the journal is append-only, so a retry cannot be expressed by rewriting
the failure away.

A retry also cannot reuse `Runner::run_task`: that starts with
`PreflightStarted`, which `state.rs` accepts only from `Queued`, and it runs
the task's normal protocol with the normal context rather than a failure
bundle. The bounded loop in `Runner::remediate` is private, needs a live
worktree from the failed attempt (every run removes it), and is driven by an
`Error` value, not a recorded failure.

## Decision

Add one event, `RetryStarted { attempt: AttemptId }`, and one transition:
`Failed` accepts it and moves to `Remediating { attempt, phase: Implement }`.
Every other (state, event) pair involving `Failed` stays invalid, and no other
state accepts `RetryStarted`.

`Runner::retry_task` runs preflight against the world as it is now (a failure
here records nothing against the task), takes the lock and a fresh worktree,
journals `RetryStarted` for the next evidence attempt id, then runs exactly
one round of the remediation machinery: the agent in a new session prompted
with `bundle(...)`, the completion gates from scratch, publication. Success
records `TaskDone`; failure records `TaskFailed`, returning the task to
`Failed` so it can be retried again.

The bundle's gate output is the failing gates the journal holds for the most
recent attempt; a failure that ran no gate contributes its recorded detail as
one synthetic failing gate, the same stand-in `Runner::remediate` uses.

## Alternatives considered

- **`Failed` accepts `Resumed` and returns to `Queued`.** Reuses an event, but
  a requeued task starts with the normal protocol and normal context, so
  nothing is seeded with the failure bundle, and `Resumed` would mean two
  different things depending on the state.
- **Retry as a mode of `run_task`.** Needs `PreflightStarted` from `Failed`,
  which widens `Failed`'s edges more than one event does, and blurs the
  distinction between a first attempt and a remediation.
- **Rewrite the task's journal.** Forbidden: the journal is the append-only
  source of truth, and "retry does not edit anything".

## Consequences

`Failed` is no longer wholly terminal: `TaskState::is_terminal` still says it
is (nothing happens without a human), but the transition table now has one
edge out of it, and the allowed-pairs guard in `state.rs` lists it.
`recovery::reconcile` needs no change: a retry interrupted mid-flight is
`Remediating`, which it already reconciles.

A retry is a single round. Repeating remediation with the breaker and bounds
is `retry` again, by a human, which keeps each attempt's evidence and each
decision visible.

The runner does not yet journal `TaskFailed` when `run_queue` sees a task
fail; a task stranded mid-attempt by such a failure is not `Failed` and is
not retryable. Only a journaled failure (a failed preflight, or a failed
retry) is. Closing that gap is a separate change to `run_queue`.

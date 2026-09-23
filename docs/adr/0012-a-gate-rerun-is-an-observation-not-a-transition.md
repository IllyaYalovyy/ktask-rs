# 0012. A gate rerun is an observation, not a transition

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T120's `rerun-gate --task <id> [--gate <kind>]` runs a gate against the
current worktree "and journals it". The journal already has `GateStarted` and
`GateFinished`, but `state::apply` accepts them only while an attempt is
running, remediating or verifying. A rerun is asked for precisely when none of
those holds: the task is failed, paused for an interruption, or waiting with
its worktree still on disk. Journaling a `GateFinished` there would be an
`InvalidTransition`, and because every command derives a task's state by
folding its events through `apply` (`read_queue`), one such event would make
the whole queue unreadable.

A rerun also differs in kind from those events. It does not belong to an
attempt, it changes nothing about the task's custody, and it must never be
mistaken by recovery or `status` for the attempt's own gate evidence.

## Decision

Add one event, `GateRerun { result: GateResult }`. `apply` handles it before
delegating: every state that is not final (`Done`, `Acknowledged`,
`Cancelled`) accepts it and returns itself unchanged; the final states reject
it like any other event. `Failed` accepts it, as does every `Paused` reason.

`rerun-gate` runs each gate through `run_gate_at` — the call
`run_completion_set` itself makes, so a rerun sees the same working directory
and `KTASK_BASE_SHA` a run would — and journals one `GateRerun` per gate,
after checking it against `apply`. The result it prints is the value it
journaled. Nothing is cached anywhere on the path: there is no lookup to
bypass, and a test flips a gate from pass to fail between two reruns to keep
it that way.

The worktree is the task's `task-<id>` worktree. A task with none (the runner
removes it on every path that finishes an attempt) is a usage error, not a
fallback to some other checkout: a gate run against the wrong tree would
report on the wrong code. The base commit is the most recent `PreflightPassed`
in the task's journal.

A task whose attempt is still in flight and whose supervisor is alive is
refused. Both would share one worktree, and a gate that formats or builds
would race the agent. `interrupt` first is the way through.

## Alternatives considered

- **Reuse `GateStarted`/`GateFinished` and widen `apply`.** Muddles the
  meaning of events that are an attempt's evidence, and would need to accept
  `GateFinished` in `Done` or refuse reruns for finished tasks.
- **Journal them with no task id.** Queue-level events are skipped by replay,
  so nothing breaks, but the record would not say which task's worktree was
  checked, which is the point of it.
- **Bracket the run with a started event, as `gate_phase` does.** The rule
  "journal before the side effect" protects custody transitions. A rerun has
  no custody effect to recover from: killed mid-gate, it leaves the journal
  exactly as it was, which is the truth.
- **Fall back to the project root when there is no worktree.** See above.

## Consequences

`EventKind` has twenty-nine variants and every exhaustive match over it gained
an arm. `status` and `run`'s progress narration ignore `GateRerun`: the first
does not read gate events, and the second has no run to narrate it in.

Reruns are visible in the journal and the log (`gate rerun: Verify failed`),
but nothing yet turns a rerun's outcome into a task's outcome: a passing
rerun does not un-fail a failed task. That stays `retry`'s job.

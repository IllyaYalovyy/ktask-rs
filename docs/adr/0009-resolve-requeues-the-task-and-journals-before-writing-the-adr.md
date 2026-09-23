# 0009. `resolve` requeues the task and journals before it writes the ADR

- **Status:** accepted
- **Date:** 2026-09-23

## Context

`ktask-rs resolve` answers a `waiting_input` question: the answer becomes an
ADR in `docs/adr/` and reaches the context of later tasks (`docs/CONTRACT.md`
section 3, T117). The journal needs an event for it, and the transition table
needs an edge out of `Paused { Input }` other than `Resumed`.

`Resumed` returns a task to `resume_to`, the state it was paused from — for an
input pause, `Running` mid-attempt. But the runner only starts `Queued` tasks
(`next_runnable`), every run removes its worktree, and a provider session is
never resumed (`docs/CONTRACT.md`, `retry`). A task handed back as `Running`
would be stranded: nothing would continue it.

Two things also have to happen for one answer: an event in the journal and a
file in the repository. `VISION.md` §6 requires every transition to be
journaled before its side effect.

## Decision

Add `DecisionResolved { adr_path, answer }` (already in the catalog in
`docs/DESIGN.md`). `Paused { Input }` accepts it and moves to `Queued`; every
other state rejects it. The task then starts over with a fresh attempt, and
its context now includes the new ADR. `Resumed` remains legal on an input
pause, unchanged.

`resolve` journals `DecisionResolved` first and writes the ADR second. The
event carries the whole answer, so a write that fails or is interrupted costs
a file, not the decision; the command reports the failure, including the
answer, and exits 1. The ADR path is stored relative to the repository root,
numbered one past the highest `NNNN-*.md` in `docs/adr/`, and written with
`create_new` so an existing record is never overwritten. The ADR is redacted
as a whole before it is written, since unlike the journal it lands in the
repository.

The editor plumbing `add` used (a scratch file in the state directory, `sh -c`
so `$EDITOR` may carry arguments) moves to `cmd/editor.rs`, shared by both
commands. `resolve` opens the editor on the question inside an HTML comment
that is stripped from the answer.

`ack` records `GateAcknowledged` with `$USER` (else `$LOGNAME`, else
`unknown`) and the current time, and runs nothing.

## Alternatives considered

- **`DecisionResolved` returns to `resume_to`.** Reuses the `Resumed` shape,
  but the state it returns to is not one the runner starts from, so the task
  would need a second command to become runnable again.
- **Write the ADR first, then journal.** A crash between the two leaves a
  file the journal knows nothing about, and the next `resolve` numbers past
  it: a duplicate decision, found by nobody. Journal-first fails in the
  direction the answer in the event can repair.
- **Journal an intent event, then write, then journal completion.** Two
  events for one answer, and a new state for the gap between them, to close a
  window that the answer-in-the-event already makes harmless.

## Consequences

`resolve` does not commit the ADR, and preflight refuses a working tree with
untracked files (`VISION.md` §10): after `resolve`, `resume` fails the task's
preflight until a human commits `docs/adr/`. The command says so on stderr.
Whether `resolve` should commit (and on which branch, with what message, and
whether it pushes) is a git-state decision this task does not take; it needs
one before the resolve-then-resume path is seamless.

An ADR lost after its resolution was journaled is not rebuilt automatically.
The journal holds everything needed (the question in `DecisionRaised`, the
answer and path in `DecisionResolved`), so a recovery step could.

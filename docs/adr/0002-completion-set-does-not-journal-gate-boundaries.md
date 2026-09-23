# 0002. `run_completion_set` does not journal gate start/finish

- **Status:** accepted
- **Date:** 2026-09-22

## Context

T085 asks `gate.rs` for `run_completion_set(profile, root, base_sha, bus)`,
"journaling each start and finish" of the format, lint, build, verify and
privacy gates it runs, and scopes the task to `gate.rs` alone.

That combination cannot be honoured literally, for the same three reasons
`docs/adr/0001-defer-gate-output-on-the-bus.md` already gave `run_gate`:

1. **There is still no `EventKind` payload for a gate boundary.**
   `event.rs`'s module doc lists `GateStarted`/`GateFinished` among the
   variants "deliberately absent because their payload names a type no
   earlier task has defined," to be "added by the task that defines its
   payload type, which also adds its arm to the transition function." That
   task is not this one: this task's files are `gate.rs` only, and adding a
   catalog entry means editing `event.rs` and `state.rs` too.
2. **`Bus::publish` is still `Recorder`'s alone to call.** `events.rs`:
   "nothing can reach a frontend that is not already durable." A
   `run_completion_set` that published straight to `bus` would reintroduce
   exactly the gap ADR 0001 refused to open for `run_gate`.
3. **`run_completion_set` cannot build the envelope even if the payload
   existed.** Its signature carries no `task_id` and no way to obtain the
   journal's next `seq`; both belong to `Recorder::record`, not to a
   gate-running function called with a bare `Option<&Bus>`.

`run_completion_set` calls `run_gate` once per gate in the completion order,
and `run_gate` itself already accepts `bus` without publishing through it,
per ADR 0001. Nothing about composing several `run_gate` calls changes any
of the three points above.

## Decision

`run_completion_set` forwards `bus` unchanged to every `run_gate` call it
makes and does not otherwise touch it. No new `EventKind` variant, no new
`Bus::publish` call site, no journal write happens in this function. The
mechanical part of the task — running format, lint, build, verify and
privacy in a fixed order, short-circuiting on the first failure, always
reaching the mandatory `Verify` gate when nothing earlier failed, and
returning every `GateResult` gathered so far — is implemented in full;
"journaling each start and finish" is the one clause this task asked for
that is not.

A test
(`gate::tests::run_completion_set_tests::a_bus_is_forwarded_but_nothing_is_published_to_it_yet`)
pins this down the same way ADR 0001's test does for `run_gate`: a
subscriber attached to the `bus` passed in sees zero events after a
successful run.

## Alternatives considered

- **Add `GateStarted`/`GateFinished` to `EventKind` and record them here.**
  Rejected: out of this task's declared file scope (`gate.rs` only), and a
  durable-data decision — what a gate-boundary event carries, whether
  `AgentOutput`-sized gate output belongs in the journal at all — that a
  single-file mechanics task should not make unilaterally.
- **Take a `Recorder` and `task_id` instead of `bus`, diverging from the
  signature the task specifies.** Rejected: the task names the exact
  signature (`bus: Option<&Bus>`), and changing it would break the one
  contract this task is required to honor precisely.
- **Silently drop the journaling clause without recording why.** Rejected:
  `docs/PROCESS.md` treats scope decisions as something to report, not
  decide quietly, and a future reader re-deriving this reasoning from
  scratch is exactly what an ADR exists to prevent.

## Consequences

A frontend watching `bus` during a completion-set run still learns nothing
mid-run; it only sees the final `Vec<GateResult>` `run_completion_set`
returns, each with full `stdout`/`stderr`. True start/finish journaling for
gates needs the same catalog entry and id-threading ADR 0001 already
flagged, done as its own task with `event.rs` and `state.rs` in scope.
Whoever does that work should extend `run_completion_set`'s loop to call
`Recorder::record` (not `Bus::publish` directly) once a `GateStarted` /
`GateFinished` payload exists, which likely means changing this function's
signature to take a `Recorder` and `TaskId` rather than a bare `Bus`.

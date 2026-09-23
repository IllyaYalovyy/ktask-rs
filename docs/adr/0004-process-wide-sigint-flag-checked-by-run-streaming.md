# 0004. A process-wide `SIGINT` flag, checked directly by `run_streaming`

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T103 asks for durable, resumable state on interrupt: a `signal_hook` handler
sets an atomic flag, checked "between phases and inside output loops", and
on interrupt the provider's process group is terminated, `Interrupted` is
journaled, and the run returns — done-when "no child survives; the journal
ends with Interrupted; resume continues from the same task."

The task's own file list names `runner.rs`, a new `tests/interrupt.rs`, and
`Cargo.toml`. But `Runner` never holds a live child process to kill: every
real subprocess (`Claude`/`Codex`'s adapter, via
`provider::process::run_streaming`) is spawned and fully reaped inside one
synchronous `Provider::invoke` call before `Runner::run_phase` ever sees
control again. A flag checked only in `runner.rs`, between phases, can
durably journal `Interrupted` once a phase's invocation *returns* — but
cannot cut a stuck invocation short, and so cannot itself terminate "the
provider's process group" while a phase is in flight, which is the specific
guarantee the task asks for (and the "output loops" wording points at:
`run_streaming`'s own receive loop is the only "output loop" this crate
has).

`Dummy`, the provider every other `runner.rs` test drives, never spawns an
OS process at all (`Hang` is a plain `thread::sleep` on the calling thread),
so no test built only on it could ever observe a killed process group
either. Proving the actual guarantee needs a real subprocess in flight when
the signal arrives.

## Decision

`interrupt_flag()` (`runner.rs`, `pub(crate)`) installs `signal_hook`'s
`SIGINT` handler once per process, behind a `OnceLock`, and returns the
`Arc<AtomicBool>` it sets. `Runner` holds the same `Arc` and checks it at
every phase boundary in `drive_attempt` (before a phase, after it returns
whether `Ok` or `Err`, and after its gate) and once per iteration of
`run_queue`'s task loop, journaling `EventKind::Interrupted` the first time
it finds the flag set.

`provider::process::run_streaming` reads the *same* global flag directly —
by calling `interrupt_flag()` again, not by taking one as a parameter — in
the exact spot it already checks `idle_timeout`/`hard_timeout` each
`POLL_INTERVAL` tick, reusing that block's existing `kill_group` and
kill-grace-then-return machinery unchanged. This is the one change outside
the task's stated file list: without it, "terminate the provider process
group" and "checked... inside output loops" would not be true statements
about the resulting code, only about `runner.rs`'s between-phases checks.

The integration test drives a real `Provider` (`Claude`, pointed at a
`claude` stand-in resolved through a `PATH` prepended in a spawned child
process, the same technique `recovery_matrix.rs` already uses for `git`)
rather than `Dummy`, specifically so a real process group exists for the
trial to prove is gone after `SIGINT`.

## Alternatives considered

- **Thread an interrupt flag through `Provider::invoke`'s signature.**
  Rejected: touches the trait every adapter implements (`Claude`, `Codex`,
  `Dummy`, `provider::conformance`'s shared test suite, and every call site
  in `runner.rs`) for a value that is process-wide already — a parameter
  would just be a second way to name the same global, not a capability
  gained.
- **Only check between phases in `runner.rs`, and rely on
  `idle_timeout`/`hard_timeout` to eventually kill a stuck provider.**
  Rejected: this is the status quo, and it is what the task is asking to
  change — a signal caught this way is durably journaled only once the
  provider's own timeout (up to `config.attempt_timeout_secs`, 4 hours by
  default) elapses, not promptly, and "no child survives" would not hold
  until then either.
- **Give `run_streaming` its own `SIGINT` handler, independent of
  `Runner`'s.** Rejected: `signal_hook` supports registering more than one
  flag for the same signal, but two independently-registered flags for the
  same event is two sources of truth for one fact (`docs/DESIGN.md`'s
  general preference for a single source of truth); a shared accessor keeps
  the fact behind one `OnceLock`, read from both call sites.

## Consequences

A `verify_command` gate stuck mid-run (`gate::run_gate`) is not interrupted
promptly by this change — only the provider's own invocation is. A `SIGINT`
during a gate is still recorded as `Interrupted` once the gate call returns
(`drive_attempt`'s post-gate check), but the gate's subprocess runs to its
own completion or its own timeout first. Extending the same
`interrupt_flag()` check to `gate::run_gate`'s loop, if a future task widens
the guarantee to gates, is the same shape of change made here to
`run_streaming` and touches only that file.

`remediate`'s retry loop (`runner.rs`) does not yet check
`Runner::interrupted` between rounds; an interrupt during remediation is
only caught once the whole `remediate` call returns (as an error, from a
provider invocation `run_streaming` cut short) or once its own round
completes. This task's done-when is scoped to a single attempt's phases, not
remediation, so that gap is left for whichever task next touches
`remediate`'s loop.

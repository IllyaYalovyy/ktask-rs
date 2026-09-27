# Architecture

## Crates and the dependency rule

```
ktask-core      domain + use cases. No I/O. Depends on nothing in this workspace.
ktask-adapters  SQLite journal, git CLI, subprocess runner, provider CLIs, clock, filesystem.
ktask-cli       binary `ktask-rs`: argument parsing, wiring, exit codes.
ktask-tui       terminal interface.
```

Dependencies point inward only: `adapters`, `cli` and `tui` depend on `core`; `core`
depends on none of them. `cli` is the only place where adapters are chosen and wired.

## Ports

Everything `core` needs from the outside world is a trait defined in `core`:
`Journal`, `Git`, `Commands` (run a process with timeout and streaming output), `Provider`,
`Clock`. Each has a real adapter in `ktask-adapters` and an in-memory fake used by `core`'s
tests. Nothing in `core` spawns a process, opens a file or reads the environment.

## Core

- **Journal first.** Every state change is an event appended to the journal before its
  side effect. Current state is a projection of the journal, updated in the same
  transaction as the append; replaying the journal must reproduce it exactly.
- **Task state machine** — pure: `(state, event) -> state | error`.
- **Queue** — tasks with stable IDs and an explicit order key, so insert, move and
  breakdown never renumber anything.
- **Pipeline** — a task's process is configuration: a sequence of steps built from a small
  set of step kinds (agent with role/provider/model/fallback, command, human approval),
  plus bounded loops (review → fix) and failure hooks (retry, hint, escalate, stop). The
  runner executes steps; it does not know what a review, a commit or a PR is.
- **Runner** — a small orchestrator composed of separate units (step execution, failure
  classification, hooks and escalation, usage and limits, recovery), each with its own
  tests.
- **Every step outcome is an event** carrying timing, attempt number, provider, model,
  tokens, cost and classification. Status, audit and process metrics are projections of the
  journal — nothing is tracked on the side.

## Interfaces

- **TUI**: `update(App, Event) -> App` and `render(&App)` are pure. One thin loop owns the
  terminal and feeds events (keys, resize, journal events, loaded data) into `update`.
  Every screen gets its data through that loop — a screen whose data is loaded only in
  tests does not exist.
- **CLI**: parse, wire adapters, call a core use case, map its outcome to an exit code.

## State and isolation

Per-project state lives under `$XDG_STATE_HOME/ktask-rs/<project>/`, configuration under
`$XDG_CONFIG_HOME/ktask-rs/`. Nothing is written into the project's working tree except by
an agent or a configured step. Every path the tool touches comes from configuration or the
environment it was started with, so tests run the real binary with their own `HOME`,
`XDG_*` and `TMPDIR`.

## End-to-end test harness

Built first, before features. It launches the real `ktask-rs` binary in a pseudo-terminal
against a scratch project (git repo + local bare remote + `dummy` provider), sends keys,
and reads the screen. Every user-visible feature is verified through it.

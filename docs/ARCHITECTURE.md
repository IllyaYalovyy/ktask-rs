# Architecture

## Crates and the dependency rule

```
ktask-core      domain + use cases. No I/O. Depends on nothing in this workspace.
ktask-adapters  SQLite journal, git CLI, subprocess runner, provider CLIs, clock, filesystem.
ktask-cli       binary `ktask-rs`: argument parsing, wiring, exit codes.
ktask-tui       terminal interface, including the shared status presentation module.
```

Dependencies point inward only: `adapters`, `cli` and `tui` depend on `core`; `core`
depends on none of them. `cli` is the only place where adapters are chosen and wired.

## Ports

Everything `core` needs from the outside world is a trait defined in `core`:
`Journal`, `Git`, `Commands` (run a process with timeout and streaming output), `Provider`,
`Clock`, `ProjectRegistry` (the projects the tool knows, in `registry.db` under the state directory of the build's channel). Each has a real adapter in `ktask-adapters` and an in-memory fake used by `core`'s
tests. Nothing in `core` spawns a process, opens a file or reads the environment.

The `Provider` adapter is one generic implementation driven by provider configuration
(command, argument templates, output format, classification patterns), plus a small set of
output parsers (plain text, JSON lines, …). Claude Code and Codex are configurations, not
types.

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

**One application layer, two frontends.** Every action and every view — add, list, remove,
run, status, everything — is a single use-case function in `core`. The CLI command and the
TUI key that do the same thing call the same function with the same arguments. Frontends
parse input and render output; they contain no logic of their own, and nothing is
implemented twice.

Core returns typed facts. The operator-facing status words and indicators for those facts live
in the TUI crate's presentation module, which both frontends call; each frontend owns only its
layout.

- **TUI**: `update(App, Event) -> App` and `render(&App)` are pure. One thin loop owns the
  terminal and feeds events (keys, resize, journal events, loaded data) into `update`.
  Every screen gets its data through that loop — a screen whose data is loaded only in
  tests does not exist.
- **CLI**: parse arguments, wire adapters, call the use case, render its result (text or
  `--json`), map its outcome to an exit code.

## Platform

Linux is the only supported platform; macOS may follow. Everything platform-specific —
process groups and signals, pseudo-terminals, paths and directory conventions, file
locking — lives behind a port or in one adapter module, never in `core` or the frontends.
No decision may make a second platform harder to add.

## No Python

`ktask-rs` depends on Rust, git and the agent CLIs it drives — nothing else. No Python, in
any form: no script in the repository, no build or test step, no fixture recorder, no
development tool, no command string that starts an interpreter. A project that `ktask-rs`
orchestrates may be written in anything; the tool itself never is. A structure test refuses
any `.py` file and any `python` in a tracked file.

## State and isolation

Per-project state lives under `$XDG_STATE_HOME/<channel directory>/<project>/`, configuration under
`$XDG_CONFIG_HOME/<channel directory>/`. Nothing is written into the project's working tree except by
an agent or a configured step. Every path the tool touches comes from configuration or the
environment it was started with, so tests run the real binary with their own `HOME`,
`XDG_*` and `TMPDIR`.

## Channels

Every binary is built for one channel, `dev` or `user`, fixed at build time and never decided at
runtime. The channel names the directory a binary keeps its world in: `ktask-rs-dev` for `dev`,
`ktask-rs` for `user`, under both the state home and the config home. A build from the
repository and the installed tool therefore never see each other's registry, queues or
settings, even when started in the same directory.

- `dev` is the default for every cargo build, test, run and install.
- `user` comes only from the installer for the user tool, which sets `KTASK_RS_CHANNEL=user`; `build.rs`
  reads it and fails the build for any other value but `dev` or `user`.
- `build.rs` also records the short git commit and whether the tree was dirty.
  `ktask-rs --version` prints `ktask-rs <version> <channel> <commit>`, `-dirty` appended when so.
- The channel is a constant of the build. Nothing at runtime inspects the binary's path or the
  environment to choose one. The mapping from channel to directory lives in one place, `state.rs`
  of `ktask-adapters`, and a structure test refuses it anywhere else.
- `project show` prints the channel and state directory; the TUI title bar reads `ktask-rs [dev]`
  on `dev` and `ktask-rs` on `user`, in every screen.
- No test and no command a `dev` binary runs reads or writes under `ktask-rs/`.

## End-to-end test harness

Built first, before features. It launches the real `ktask-rs` binary in a pseudo-terminal
against a scratch project (git repo + local bare remote + `dummy` provider), sends keys,
and reads the screen. Every user-visible feature is verified through it.

# 0005. `ktask-cli` depends on `ktask-core` by path; global output flags are a flattened `Args` struct

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T104 asks for a `clap` derive `Cli`/`Command` covering every command and
global option in `docs/CONTRACT.md` sections 2 and 3, wiring `clap`, `serde`,
`serde_json` and `ktask-core` into `ktask-cli`'s `Cargo.toml`. `clap`,
`serde` and `serde_json` were already workspace dependencies (used
elsewhere); `ktask-core` was not yet depended on by any other crate in the
workspace, so this is the first crate-to-crate edge.

Two decisions followed from actually wiring these in:

1. `--task`/`--from`/`--gate` need typed values. `ktask-core` already exports
   exactly the right newtypes — `TaskId` (`ids.rs`) and `GateKind`
   (`gate.rs`) — both `Copy`, `Eq`, and already carrying the invariants
   (`TaskId` is "1-based and matching queue order"; `GateKind` is the closed
   set of mechanical gates) the CLI would otherwise have had to redefine and
   keep in sync by hand.
2. `docs/CONTRACT.md` section 2 lists four boolean global flags: `--json`,
   `--no-color`, `--verbose`, `--quiet`. Declared as four `bool` fields on
   one `Cli` struct, `clippy::struct_excessive_bools` (part of `pedantic`,
   denied workspace-wide) fails the build: "consider using a state machine
   or refactoring bools into two-variant enums."

## Decision

`ktask-cli/Cargo.toml` depends on `ktask-core` as `{ path = "../ktask-core",
version = "0.1.0" }` — the version is required alongside the path or
`cargo-deny`'s `wildcards = "deny"` bans check rejects it as a wildcard
dependency, even though both crates always move together inside one
workspace.

`Command`'s `--task`, `--from` and `--gate` fields are typed as
`ktask_core::TaskId` and `ktask_core::GateKind` directly, parsed by two small
`value_parser` functions (`parse_task_id`, `parse_gate_kind`) rather than
CLI-local re-declarations of the same closed sets.

The four boolean flags are split across two structs instead of one: `--json`
and `--no-color` move into a `#[derive(Args)]` `OutputOptions`, flattened
into `Cli` via `#[command(flatten)]`; `--verbose` and `--quiet` stay directly
on `Cli`. Each struct now has two bools, under the lint's threshold, and the
CLI surface is unchanged — flattened fields still parse as top-level global
flags, before or after the subcommand.

## Alternatives considered

- **`#[allow(clippy::struct_excessive_bools)]` on `Cli`.** Permitted by
  `AGENTS.md` at a module/crate boundary with justification, but the bools
  here are independent orthogonal toggles with no shared state, which is
  exactly the case pedantic's own suggestion ("two-variant enums") does not
  fit — grouping them by what they actually mean (how output is rendered,
  vs. how much of it there is) is a real improvement, not a lint dodge, so
  there was no reason to reach for the suppression instead.
- **CLI-local `TaskId`/`GateKind` re-declarations, avoiding the new
  dependency edge.** Rejected: it is the exact duplication `ktask-core`
  exists to prevent (VISION.md's "one core"), and it would leave two
  independently-evolving definitions of what a gate kind or a task id is.
- **Raw `u32`/`String` for `--task`/`--from`/`--gate` instead of the
  `ktask-core` types**, deferring the typed values to whichever task first
  implements dispatch. Rejected: the parse step is exactly where a bad value
  should be rejected (exit 2, per section 1), and `docs/CONTRACT.md`
  guarantees an id is already a `TaskId` by the time anything acts on it —
  postponing that to a later task would mean re-parsing the same strings
  twice, once loosely here and once strictly there.

## Consequences

Any future change to `TaskId`'s or `GateKind`'s representation is felt by
`ktask-cli` at compile time rather than silently drifting. `ktask-core`'s
public surface is now part of `ktask-cli`'s API contract, which is the
intended shape per VISION.md's "one core, two thin frontends" — the same
edge `ktask-tui` will need when it is wired to real state instead of view
state.

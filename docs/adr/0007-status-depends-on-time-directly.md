# 0007. `ktask-cli` depends on `time` directly, for `status`'s timestamps

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T111 gives `ktask-rs status` its real body: per task, `docs/CONTRACT.md`
section 3 wants a state, a protocol, a phase, an attempt count and, for the
active task, elapsed time; `--json` additionally wants `started_at` and
`ended_at`. Those timestamps come straight from `ktask_core::Event::ts`,
which is a `time::OffsetDateTime` — `time` is already a fixed workspace
dependency (`Cargo.toml`'s `[workspace.dependencies]`), used throughout
`ktask-core` (`journal.rs`, `event.rs`, `classify.rs`, ...), but until now no
other crate in the workspace named it directly: `ktask-cli` only ever passed
opaque `ktask_core` values around.

`status` needs to *do* something with a timestamp — subtract two of them to
get elapsed time, format a `time::Duration` for the human line, and give
`time::serde::rfc3339::option` to `serde` for the `--json` form's
`started_at`/`ended_at` (mirroring `event.rs`'s own
`#[serde(with = "time::serde::rfc3339")]`, extended to `Option`). None of
that is expressible while only naming `time::OffsetDateTime` through a
`ktask_core` field: Rust's extern prelude only puts a crate's *direct*
dependencies in scope, and `ktask_core::Event`'s `pub ts: OffsetDateTime`
field does not re-export the `time` crate itself.

## Decision

`ktask-cli/Cargo.toml` adds `time = { workspace = true }` to `[dependencies]`.
This does not touch the workspace's fixed dependency set (`deny.toml`: "do
not add, remove or substitute a crate without an ADR") — `time` is already in
it, vetted and resolved — it only lets a second crate in the workspace draw
on a dependency the first crate already justified. `cmd::status` uses
`time::OffsetDateTime`, `time::Duration` and
`#[serde(with = "time::serde::rfc3339::option")]` directly; its tests use
`time::macros::datetime!` for fixtures, the same macro `ktask-core`'s own
tests already rely on.

## Alternatives considered

- **Format elapsed time and timestamps inside `ktask-core` instead**, so
  `ktask-cli` never needs `time` itself. Rejected: formatting a duration for
  a human terminal line, and choosing what `--json` looks like, are
  presentation decisions — exactly what `docs/CONTRACT.md` section 0 rule 2
  ("no behavior is implemented in a frontend") reserves for *behavior*, not
  rendering. `ktask-core` computing "1h02m03s" strings would be the
  supervisor's pure core reaching into a frontend's job.
- **`std::time` instead of the `time` crate**, avoiding a new direct
  dependency's Cargo.toml line. Rejected: `Event::ts` is a
  `time::OffsetDateTime`, not a `std::time::SystemTime` or `Instant`; using
  `std::time` for the subtraction would mean converting at the boundary for
  no benefit, and would still need `time::Duration` if a value ever had to
  cross back (as the `#[serde(with = ...)]` attribute does).

## Consequences

`ktask-cli`'s `Cargo.lock` entry for `time` now reflects direct use, not only
a transitive one through `ktask-core`; nothing else in the dependency graph
changes, since the crate was already resolved. Any later CLI command that
needs to render or compare timestamps (the TUI's own screens will need the
same eventually) can now do so without repeating this decision.

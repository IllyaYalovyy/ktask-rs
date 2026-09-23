# 0014. `ktask-tui` depends on `ratatui`, `crossterm`, `time` and `ktask-core`

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T127 builds the TUI's application skeleton: `AppEvent`, `App`, a pure
`update` and a `render` that draws into a `ratatui::Frame`. Until now
`ktask-tui` depended on `ktask-core` alone. `ratatui`, `crossterm` and `time`
are already fixed workspace dependencies (`Cargo.toml`,
`[workspace.dependencies]`; docs/DESIGN.md "Dependencies"), resolved and
checked against `deny.toml`; no crate outside that set is added.

## Decision

`ktask-tui/Cargo.toml` adds `ratatui`, `crossterm` and `time` with
`workspace = true`, alongside the existing path dependency on `ktask-core`.

- `ratatui` supplies `Frame` and the widgets `render` draws with, and the
  `TestBackend` that lets `render` be checked headlessly.
- `crossterm` supplies `KeyEvent`, the payload of `AppEvent::Key`. It is named
  directly because `ratatui` does not put the `crossterm` crate itself in
  scope for a dependent.
- `time` is named directly for the same reason as in ADR 0007: `Event::ts` is
  an `OffsetDateTime`, and building or reading one needs the crate in scope.
  At present it is used by the tests that construct journal events; later
  screens that show timestamps and elapsed time will use it in the library.

## Alternatives considered

- **Leave `time` out until a screen needs it.** Rejected: the task fixes the
  dependency set for this crate, and the tests already need it to build
  `ktask_core::Event` values.

## Consequences

`Cargo.lock` gains the resolved `ratatui`/`crossterm` tree, which the build
gate's `--locked` requires to be committed with this change. The terminal
shell that will own `crossterm`'s raw-mode I/O does not exist yet; `update`
and `render` take no dependency on it beyond the `KeyEvent` value type.

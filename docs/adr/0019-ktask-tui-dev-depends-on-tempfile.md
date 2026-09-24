# 0019. `ktask-tui` dev-depends on `tempfile`

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T148's done-when needs a test in `ktask-tui` that answers a question from the
input inbox and reads back the ADR that lands in a repository. That needs a
disposable project directory holding a journal and a `docs/adr` to write to.
`tempfile` is already a workspace dependency (`Cargo.toml`,
`[workspace.dependencies]`) and a dev-dependency of `ktask-core` and
`ktask-cli`, so it is in `Cargo.lock` already.

## Decision

`ktask-tui/Cargo.toml` adds `tempfile` under `[dev-dependencies]` with
`workspace = true`. It is used by tests only; the shipped crate does not
depend on it.

## Alternatives considered

- **Directories under `std::env::temp_dir` named by hand.** Needs unique
  names and cleanup code in every test, and leaves litter when a test
  panics, which `tempfile` does not.
- **Move the test to `ktask-cli`.** The verification command for T148 runs
  the `ktask-tui` tests by name; a test elsewhere would not be run by it.

## Consequences

`Cargo.lock` records one new edge for `ktask-tui`; no package is added.

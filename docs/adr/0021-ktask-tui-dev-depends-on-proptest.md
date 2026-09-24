# 0021. `ktask-tui` dev-depends on `proptest`

- **Status:** accepted
- **Date:** 2026-09-24

## Context

`docs/CONTRACT.md` §5 requires a property test that feeds arbitrary event
sequences to the interface and asserts it never panics and never hangs (T154).
The sequences are generated, and a failing one has to shrink to a short
sequence a person can read. `proptest` is already a workspace dependency
(`Cargo.toml`, `[workspace.dependencies]`) and a dev-dependency of
`ktask-core`, so it is in `Cargo.lock` already.

## Decision

`ktask-tui/Cargo.toml` adds `proptest` under `[dev-dependencies]` with
`workspace = true`. It is used by tests only; the shipped crate does not
depend on it.

## Alternatives considered

- **A hand-written random generator over a seeded PRNG.** Reproducible, but
  it would need its own shrinking to give a minimal failing sequence, which is
  what `proptest` is for.
- **`quickcheck`.** Not in the lock file, so it would add a package where
  `proptest` adds none, and the project's other property tests are already
  written against `proptest`.

## Consequences

`Cargo.lock` records one new edge for `ktask-tui`; no package is added.

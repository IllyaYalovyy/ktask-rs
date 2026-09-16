# Quality gates

`./scripts/quality.sh` is the single entry point and the definition of done.
Every gate fails the build on violation — none of them are advisory.
Install everything they need with `./scripts/setup.sh`.

| Gate | Command | Fails on |
|---|---|---|
| `fmt` | `cargo fmt --all --check` | any deviation from `rustfmt.toml` |
| `build` | `cargo build --workspace --locked --all-targets` | compile errors; a `Cargo.lock` that would change |
| `test` | `cargo test --workspace --locked` | a failing test, including doc tests |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` | any lint in the workspace lint set |
| `doc` | `cargo doc --no-deps` with `RUSTDOCFLAGS=-D warnings` | undocumented public items, broken intra-doc links |
| `deny` | `cargo deny check bans licenses sources` | disallowed license, wildcard or unknown-source dependency |
| `unused` | `cargo machete` | a declared dependency nothing uses |
| `typos` | `typos` | misspellings in prose and identifiers |

Run a subset while iterating: `./scripts/quality.sh fmt clippy`.

## What is enforced, and why

**Formatting** (`rustfmt.toml`) uses stable options only, so the check behaves
identically everywhere rather than depending on a contributor's toolchain.

**Lints** are declared once in `Cargo.toml` under `[workspace.lints]` and
inherited by every crate, so a crate cannot quietly opt out. Beyond
`clippy::all` and `clippy::pedantic`, the set encodes opinions specific to this
project:

- `unsafe_code = "forbid"` — not `deny`; it cannot be overridden locally.
- `missing_docs` — a public item without documentation fails the build. The
  documentation requirement is enforced, not requested.
- `unwrap_used`, `expect_used`, `panic`, `indexing_slicing` — **a supervisor
  that panics loses the run it was supervising.** Errors are values here.
  `clippy.toml` allows all of these in tests, where they are the clearest way
  to assert.
- `print_stdout`, `print_stderr` — library code returns values and errors
  instead of emitting them, which is what keeps the core testable without
  capturing output. `ktask-cli` is the documented exception: printing is that
  crate's purpose.
- `todo`, `unimplemented`, `dbg_macro` — scaffolding must not reach a commit.

`clippy::cargo` is deliberately not enabled: it demands crates.io publishing
metadata this tool does not need. Dependency hygiene lives in `deny.toml`.

## Suppressing a lint

A lint may be suppressed only at an architectural boundary, at module or crate
level, with a comment explaining why the rule does not apply there — as
`ktask-cli` does for printing. Suppressing a lint on the line that triggered it,
to make a specific violation go away, is weakening a gate and fails the task
that did it.

## Not in the gate suite

- **Advisories** (`cargo deny check advisories`) need network access and a
  current advisory database, so they are not part of the offline gate run.
  Run them separately and deliberately.
- **Mutation testing** (`cargo mutants`) measures whether the tests actually
  constrain the code rather than merely executing it. It is far too slow for
  an inner loop; it is run against finished work. Surviving mutants are a
  finding, not a formality.

# ktask-rs

A Rust rewrite of `ktask`: a supervisor for unattended AI coding work.
Ships as a single binary, `ktask-rs`.

Other tools automate agents. ktask guarantees that ordered work was actually
verified, published, and recoverable — without leaking its operational context
into the repository. See [VISION.md](VISION.md) for the full design.

## Getting started

[docs/GUIDE.md](docs/GUIDE.md) takes you from install to a drained queue with
the built-in `dummy` provider: `init`, `add`, `plan lint`, `run` and `status`,
each with its real output. The guide is run by the test suite, so what it
shows is what the commands print.

## This branch

`template` is the **seed** every implementation run starts from. It is
deliberately minimal and deliberately green: the workspace compiles, the tests
pass, and every gate in `scripts/quality.sh` succeeds before any feature work
begins. That is what makes "prove the project was green before you started"
checkable from the very first task.

```bash
git switch -c my-run template
./scripts/check-prereqs.sh   # what must be installed first
./scripts/quality.sh      # verify the seed is green
```

Nothing on this branch implements ktask yet. The crates hold placeholder
markers that the first tasks replace.

## Layout

| Path | Purpose |
|---|---|
| `VISION.md` | the design this implementation must satisfy |
| `AGENTS.md` | working rules for whoever (or whatever) writes the code |
| `docs/PROCESS.md` | definition of done, commits, ADRs, scope |
| `docs/TESTING.md` | test layers, and the mandatory TUI coverage |
| `docs/adr/` | architecture decision records — the only operational docs kept in-repo |
| `scripts/check-prereqs.sh` | reports what must be installed before any task can run |
| `scripts/quality.sh` | the single entry point for every mechanical gate |
| `docs/GUIDE.md` | getting started: install, init, add, plan lint, run, status |
| `docs/OPERATING.md` | every failure class and pause state: exit code, what happens, what you do |
| `docs/CONTRACT.md` | the CLI and TUI surface: commands, exit codes, screens, keys |
| `docs/QUALITY.md` | what each gate enforces, and why |
| `rustfmt.toml`, `clippy.toml`, `_typos.toml` | static analysis configuration |

There is no pinned toolchain file: the project builds with the Rust already on
the machine, provided it meets the `rust-version` minimum.
| `deny.toml` | dependency license and source policy |
| `crates/ktask-core` | state machine, journal, gates, providers — pure logic, no I/O |
| `crates/ktask-cli` | headless command-line interface; builds the `ktask-rs` binary |
| `crates/ktask-tui` | terminal interface, headlessly testable |
| `tests/scenarios/` | end-to-end acceptance scenarios |

## Quality gates

```bash
./scripts/quality.sh                 # fmt build test clippy doc deny unused typos
./scripts/quality.sh fmt clippy      # a subset, while iterating
```

Eight gates, every one of them failing the build on violation: formatting,
compilation against a locked dependency set, tests, `clippy` with
`-D warnings` over `all` + `pedantic` plus project-specific denials,
documentation (undocumented public items and broken links fail),
dependency licenses and sources, unused dependencies, and spelling.
`docs/QUALITY.md` explains each one.

Gates are never to be weakened to get a green result, and a gate whose tool is
missing reports as failed rather than skipped — an unverified gate has not
passed. `./scripts/check-prereqs.sh` tells you what is missing and how to install it.
It installs nothing.
`cargo deny check advisories` needs network access and is run separately from
the offline gate set.

## License

GPL-3.0-only. See [LICENSE](LICENSE).

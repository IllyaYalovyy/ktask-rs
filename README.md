# ktask-rs

A Rust rewrite of `ktask`: a supervisor for unattended AI coding work.

Other tools automate agents. ktask guarantees that ordered work was actually
verified, published, and recoverable — without leaking its operational context
into the repository. See [VISION.md](VISION.md) for the full design.

## This branch

`template` is the **seed** every implementation run starts from. It is
deliberately minimal and deliberately green: the workspace compiles, the tests
pass, and every gate in `scripts/quality.sh` succeeds before any feature work
begins. That is what makes "prove the project was green before you started"
checkable from the very first task.

```bash
git switch -c my-run template
./scripts/quality.sh
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
| `scripts/quality.sh` | the single entry point for every mechanical gate |
| `deny.toml` | dependency license and source policy |
| `crates/ktask-core` | state machine, journal, gates, providers — pure logic, no I/O |
| `crates/ktask-cli` | headless command-line interface |
| `crates/ktask-tui` | terminal interface, headlessly testable |
| `tests/scenarios/` | end-to-end acceptance scenarios |

## Quality gates

```bash
./scripts/quality.sh              # fmt, build, test, clippy, deny
./scripts/quality.sh fmt test     # a subset
```

Gates are not advisory and are never to be weakened to get a green result. A
gate whose tool is not installed reports as failed, because it is unverified;
`cargo-deny` is the one gate needing a separate install.
`cargo deny check advisories` needs network access and is run separately from
the offline gate set.

## License

GPL-3.0-only. See [LICENSE](LICENSE).

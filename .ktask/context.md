# Project context

You are implementing **ktask-rs**: a Rust rewrite of `ktask`, a supervisor for
unattended AI coding work. Read these before doing anything else:

- `VISION.md` — the design you are implementing. It is authoritative. If a task
  appears to contradict it, that is a finding to report, not a decision to make.
- `AGENTS.md` — how work is done here, and the reporting contract.
- `docs/PROCESS.md` — definition of done, commits, ADRs, scope.
- `docs/TESTING.md` — test layers, crash-recovery testing, mandatory TUI coverage.

## What this project is

A supervisor that guarantees ordered work was actually verified, published and
recoverable — without leaking its operational context into the repository.
Other tools automate agents; this one proves outcomes.

Three properties carry more weight than anything else, and no task may trade
them away for speed:

1. **The TUI is the primary interface.** Not a rendering of CLI output. The CLI
   stays complete for scripting, adding tasks and checking status.
2. **Recovery is a feature, not an edge case.** Every transition is journaled
   before its side effect, so an interruption resolves to a known state.
3. **Nothing is done on an agent's say-so.** Completion is mechanical.

## Working rules

- `./scripts/quality.sh` is the definition of done. Never weaken a gate to get
  a green result — no `#[allow]` to silence clippy, no `#[ignore]` on a failing
  test, no deleted assertions. Weakening a check fails the task even if
  everything then passes.
- Test-first for behavior-bearing code: write the failing test, watch it fail,
  then make it pass.
- Tests must assert behavior, not merely execute code. Suite quality is scored
  by mutation testing.
- Record real design decisions as ADRs in `docs/adr/`. Adding a dependency is a
  design decision.
- Do the task you were given and nothing more. Note adjacent work in the report
  instead of doing it.
- `.ktask/queue/` and `.ktask/logs/` are operational state: never commit them.
  The configuration files in `.ktask/` are versioned and must not be edited.

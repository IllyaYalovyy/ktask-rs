# .ktask — supervisor configuration

Versioned, and part of the template so every run is configured identically:

| File | Purpose |
|---|---|
| `config.toml` | budgets, attempt policy, completion proof, gate command |
| `prompt.md` | prompt template wrapped around each task (`{{TASK}}`) |
| `context.md` | project context prepended to every task |
| `tasks.md` | the queue — tasks separated by `---`, executed in order |

Not versioned (see `.gitignore`): `queue/` and `logs/` are per-run state.

Two settings are supplied per run by the harness and are absent from
`config.toml` on purpose — `executor` and `model`. Everything else is fixed:
changing it changes the experiment.

## Task format

`tasks.md` holds tasks separated by lines containing only `---`. Status markers
(`[DONE]`, `[FAIL]`, `[INPUT]`) are written back by the supervisor; do not edit
them by hand. Each task states its own acceptance criteria:

```markdown
## Journal every transition before its side effect

**Outcome:** state transitions are persisted before the effect they describe,
so an interruption resolves to a known state rather than an ambiguous one.

**Done-when:** the journal records a transition before the side effect runs;
replaying the journal reproduces materialized state exactly; killing the
process between the two leaves a recoverable state.

**Verify:** `cargo test -p ktask-core journal::` and `./scripts/quality.sh`

**Refs:** VISION.md §6, docs/TESTING.md
```

A task whose `Verify` cannot be run mechanically is not ready to be queued.

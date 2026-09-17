# .ktask — supervisor configuration

Versioned, and part of the template so every run is configured identically:

| File | Purpose |
|---|---|
| `config.toml` | budgets, attempt policy, completion proof, gate command |
| `prompt.md` | prompt template wrapped around each task (`{{TASK}}`) |
| `context.md` | project context prepended to every task |
| `tasks.md` | the queue — supplied per run, **not versioned** |

Not versioned (see `.gitignore`): `queue/` and `logs/` are per-run state, and
so is `tasks.md`. The supervisor rewrites the queue in place as tasks finish,
prefixing lines with `[DONE]` or `[FAIL]`. Tracking it would dirty the working
tree after every task, sweep progress markers into the commits under review,
and — worst — put an agent's own task list inside its write scope.

Two settings are supplied per run by the harness and are absent from
`config.toml` on purpose — `executor` and `model`. Everything else is fixed:
changing it changes the experiment.

## Task format

Tasks are authored as Markdown and imported once with `ktask-rs add --file`.
The queue then lives in the database, so nothing is ever written back into the
document and it stays ordinary Markdown: headings work, code fences work, and
no line is reserved.

```markdown
## T014 Journal every transition before its side effect

**Outcome:** state transitions are persisted before the effect they describe,
so an interruption resolves to a known state rather than an ambiguous one.

**Done-when:** the journal records a transition before the side effect runs;
replaying the journal reproduces materialized state exactly.

**Verify:** `cargo nextest run -p ktask-core -E 'test(/journal::/)'`

**Refs:** VISION.md section 6
```

A task whose `Verify` cannot be run mechanically is not ready to be queued.

## When a task cannot be completed

Report `KTASK_RESULT: FAILED` with the evidence and stop. Do not weaken a gate,
do not report success that the gates do not support, and do not keep retrying
the same approach. A recorded failure is a usable result; a fabricated success
is not, and thrashing burns the budget for every task after it.

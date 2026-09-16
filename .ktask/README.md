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

`tasks.md` holds tasks separated by lines containing only `---`, executed in
order. Status markers (`[DONE]`, `[FAIL]`, `[INPUT]`) are written back by the
supervisor; do not edit them by hand.

**Three parser rules constrain how a task may be written.** They are not style
preferences — breaking them silently corrupts the task the agent receives:

1. **No line may begin with `#`.** The parser strips every such line before the
   agent ever sees it, with no awareness of code fences. That rules out
   Markdown headings, shell comments at the start of a line, and Rust
   attributes such as `#[test]` in column one. Use bold labels for structure,
   and indent anything that must begin with `#`.
2. **No line inside a task may be exactly `---`.** It would split the task in
   two. Avoid horizontal rules and YAML front matter.
3. **The first line is the task's identity.** It is shown in `status`, used to
   locate the task when marking it done, and its first word keys any
   task-specific verification command. Start it with a stable identifier.

A task therefore looks like this:

```
T014 Journal every transition before its side effect

**Outcome:** state transitions are persisted before the effect they describe,
so an interruption resolves to a known state rather than an ambiguous one.

**Done-when:** the journal records a transition before the side effect runs;
replaying the journal reproduces materialized state exactly; killing the
process between the two leaves a recoverable state.

**Verify:** `cargo test -p ktask-core journal::` and `./scripts/quality.sh`

**Refs:** VISION.md section 6, docs/TESTING.md
```

A task whose `Verify` cannot be run mechanically is not ready to be queued.

## When a task cannot be completed

Report `KTASK_RESULT: FAILED` with the evidence and stop. Do not weaken a gate,
do not report success that the gates do not support, and do not keep retrying
the same approach. A recorded failure is a usable result; a fabricated success
is not, and thrashing burns the budget for every task after it.

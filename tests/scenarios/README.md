# Public scenario subset

End-to-end acceptance scenarios driven by the `dummy` provider. These are the
scenarios you can see; a hidden subset is used for scoring, so make the
implementation correct rather than fitted to these cases.

## The scenario file

A scenario is one TOML document, named by the `dummy_scenario_path` setting,
that declares what each provider session does instead of leaving it to chance.
It is a list of steps, read in the order they are written:

```toml
[[steps]]
on_task = 1
outcome = "success"
stdout = "implemented the thing\n"

[steps.files]
"src/lib.rs" = "// written by the dummy provider\n"

[[steps]]
on_attempt = 2
outcome = "failure"
exit_code = 1
delay_ms = 250
```

Each step declares:

- **one cue** — `on_task`, answering every session of that task, or
  `on_attempt`, answering one numbered attempt. A step with neither would never
  run, and a step with both is two rules claiming one step.
- **`outcome`** — one of `success`, `failure`, `hang`, `limit` or
  `needs_input`: the five responses VISION.md §12 gives the provider. A sixth
  word is refused when the file is read, and the error names the step by its
  position and by the session it answers.
- **`stdout`** (optional) — what the session prints. A `limit` names when it
  clears and a `needs_input` asks its question in this text.
- **`exit_code`** (optional) — the status to report, defaulting to the one the
  outcome word implies: 1 for `failure`, 0 otherwise. Declared beats implied in
  both directions, so `outcome = "failure"` with `exit_code = 0` stages the
  agent that reported success and was not done.
- **`delay_ms`** (optional) — how long the session waits before it answers.
  Absent means no delay.
- **`files`** (optional) — path to contents, written into the session's working
  directory before it answers. Paths are below that directory and stay there: an
  absolute path, or one that climbs out with `..`, is a load error, because a
  scenario must not be able to touch a checkout it was never given.
  A path is a TOML key, so a path containing a dot has to be quoted —
  `"src/lib.rs" = "…"` is a path, and unquoted it is three nested tables. The
  writer quotes them for you.

`hang` is the one outcome whose point is that no answer arrives; it is how a
scenario exercises the attempt watchdog and crash recovery. A key the format does
not define, and a scenario with no steps at all, are both load errors — reading a
misspelling and ignoring it is how a scenario ends up silently not testing the
thing it was written for.

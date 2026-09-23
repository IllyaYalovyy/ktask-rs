# 0006. `doctor` gets its own `RunOutcome` variant; `--json` is threaded through `dispatch`

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T110 asks for `ktask-rs doctor`: five checks (provider availability, git,
toolchain, state directory permissions, journal health), one line each with
status and remedy, `--json` support, and exit 1 when any check fails
(`docs/CONTRACT.md` §3).

Two things `cmd::doctor` needs did not exist yet:

1. **An exit code.** `RunOutcome` (`ktask-core`) is "the typed result of a
   queue run or any single CLI command" and its own doc comment says "every
   outcome `docs/CONTRACT.md` §1 documents has a variant here." Exit code 1
   is `TaskFailed { task: TaskId }` — but doctor's failure is not about any
   one queued task, and `exit.rs` carried an explicit invariant, checked by
   a test, that only `TaskFailed` maps to exit 1.
2. **Access to `--json`.** `--json` is a global option (`Cli.output.json`),
   not a field of `Command`, and `cmd::dispatch`'s signature had no way to
   pass it to an arm. Every `cmd::*::run` so far ignored it because every
   command so far was still a placeholder.

## Decision

Added `RunOutcome::CheckFailed { detail: String }` to `ktask-core`,
documented as the "command ran to completion and found something wrong,
but not about one task" counterpart to `TaskFailed`. `exit::code_for` maps
both `TaskFailed` and `CheckFailed` to 1; the invariant test in `exit.rs`
(`only_task_failed_maps_to_the_failure_code`) was renamed and updated to
`is_task_failed || is_check_failed` rather than dropped.

`cmd::dispatch` gained a fourth parameter, `json: bool`, sourced from
`cli.output.json` in `main.rs`. Only `doctor::run` consumes it today; every
other arm's own function signature is unchanged, so this is a one-line
addition per future command rather than a second refactor.

`doctor::run(project, config, json)` computes all five `CheckResult`s
first, then either prints one line each (`render::out`) or serializes the
whole array once (`json::emit_json`) — never both — and folds the results
into `Drained` or `CheckFailed`.

Each `check_*` function takes its real-world dependency already reduced to
a value or a small closure (mirroring `paths::state_root_with`'s injected
`env` closure): `check_provider`/`check_git`/`check_toolchain` take a probe
closure returning `Option<String>` (spawn succeeded vs. could not be
spawned at all), so a test can assert the "git not found" branch without
needing an environment where git is actually missing. `check_state_dir` and
`check_journal` take a real path and exercise the real filesystem / a real
`Journal::open`, since a failure there is reproducible directly (an
unwritable directory, a corrupt journal file) without a mock.

## Alternatives considered

- **Reuse `TaskFailed` with a sentinel `TaskId`.** Rejected: no task ID is
  meaningful here, and a fabricated one would be actively misleading to
  anything that later reads `RunOutcome` (logs, the TUI) expecting it to
  name a real queued task.
- **Call `std::process::exit(1)` directly from `cmd::doctor::run`.** Rejected:
  every other command's exit code flows through `main.rs`'s single
  `exit::code_for(&outcome)` call after `dispatch` returns; a direct exit
  call bypasses that, is untestable (it would kill the test process), and
  breaks the "nothing reasons about exit statuses except the CLI, in one
  place" invariant `RunOutcome`'s doc comment states.
- **A global `AtomicBool` for `--json`, mirroring `render.rs`'s
  `COLOR_DISABLED`/`QUIET`/`VERBOSE`.** Considered since it would avoid
  touching `dispatch`'s signature at all. Rejected: `--json` changes what a
  command computes and returns (the whole `--json` vs. human-readable
  branch in `doctor::run`), not just how a fixed message is styled the way
  color/quiet do to a single already-decided line; threading it as an
  explicit parameter keeps that decision visible at the call site instead
  of hidden in global state two modules away.

## Consequences

Every later command that reports state (`status` for T111, `plan lint` for
T113, and so on) can read `--json` the same way: add `json: bool` to its own
`run` and pass `dispatch`'s existing `json` through. A future command that
needs its own non-task, non-usage failure (for instance `plan lint`
reporting several malformed tasks at once) has `CheckFailed` already
available rather than needing another `RunOutcome` variant, provided its
failure is genuinely "the command completed and found problems" and not a
pause or a usage error.

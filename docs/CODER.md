# Coder

Read `VISION.md` and `ARCHITECTURE.md`. They win over the task text; if the task
contradicts them, stop and report the contradiction.

## Done means

- **Wired.** The change is reachable from the real binary. Code that only tests call is
  not done. If a task builds a piece whose wiring belongs to a later task, say so in the
  report — do not call it done.
- **Tested end to end.** Logic gets unit tests against the port fakes. On top of that,
  through the real binary:
  - **CLI**: every command and every option, in and out — valid input, invalid input,
    text and `--json` output, exit codes, and the resulting state.
  - **TUI**: every screen, key by key, in and out — every way in, every action, every way
    back — asserting what the screen shows after each step.
  - **Both ways round**: what one frontend does, the other shows.
  Tests do not have to be written first. They do have to exist, and each must fail if the
  behaviour it names breaks.
- **No duplication between frontends.** A CLI command and its TUI action call the same
  use-case function. Logic in a frontend is a defect.
- **Green.** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, run in the foreground, all passing.

## Rules

- `core` does no I/O. New outside-world needs become a port, an adapter and a fake.
- Tests are hermetic: temp directories only; never `set_current_dir`, never set process
  environment variables, never touch the real home directory. Child processes get their
  own `HOME`/`XDG_*`/`TMPDIR`. No `sleep` to wait for something — wait for the condition,
  with a timeout.
- Never make a check pass by weakening it: no `#[ignore]`, no `#[allow]` on the offending
  line, no deleted or loosened assertion, no narrowing a test until it passes.
- Do what the task asks. Report adjacent problems instead of fixing them.
- A product decision the documents do not answer is not yours to make — stop and ask.

## Report

What changed, how it was verified (commands and their result), and what is not done.

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
- **A TUI screen or action that has not been clicked through, in and out, by a test
  driving the real binary with keystrokes in a pseudo-terminal, is not tested and not
  done.** Unit tests of `update`, snapshots of `render` and harnesses that bypass the real
  binary are useful and do not count. No exceptions.
- **No duplication between frontends.** A CLI command and its TUI action call the same
  use-case function. Logic in a frontend is a defect.
- **Green.** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, run in the foreground, all passing.

## Clean code — the goal

Judged by Robert C. Martin's criteria, not by counts:

- **One responsibility per module and per function.** A module is about one thing; if its
  name needs "and", it is two modules. A function does one thing at one level of detail.
- **Deep modules behind simple interfaces.** Put the complexity inside; keep what callers
  see small and typed.
- **Dependencies point inward.** Core states facts; the words people read are made where
  they are shown, once, for both interfaces.
- **No duplication.** Two places that must change together are one place.
- **Names that say what, not how.** A reader should not need the body to know the intent.

The workspace tests that fail a file past 400 lines or a step naming another are
**tripwires, not targets**. They say a responsibility has grown; the fix is to separate
responsibilities. Splitting a file to get under a number, or counting lines as a goal,
is a defect.

## Rules

- `core` does no I/O. New outside-world needs become a port, an adapter and a fake.
- Tests are hermetic: temp directories only; never `set_current_dir`, never set process
  environment variables, never touch the real home directory. Child processes get their
  own `HOME`/`XDG_*`/`TMPDIR`. No `sleep` to wait for something — wait for the condition,
  with a timeout.
- Never make a check pass by weakening it: no `#[ignore]`, no `#[allow]` on the offending
  line, no deleted or loosened assertion, no narrowing a test until it passes.
- A lint is fixed, never silenced — a structure test refuses any `allow` or `expect`
  attribute, anywhere, so there is no cheaper way around it than fixing what it warns about.
- **Nothing outside the task's scope is mocked, stubbed or sketched.** No placeholder
  screens, lines, commands or options for features that do not exist yet. What is in scope
  is complete, wired and tested end to end; what is not in scope is absent.
- **No code without a current use.** Nothing speculative, nothing "for later", no
  compatibility shims, no unused options or parameters. The future gets its code when it
  arrives.
- Do what the task asks. Report adjacent problems instead of fixing them.
- Every task raises the patch part of the workspace version (`version` under
  `[workspace.package]` in `Cargo.toml`, with `Cargo.lock`) by one, in the task's own commit;
  `ktask-rs --version` must show the new number. The minor part is raised only by a task
  that says so; the major part never, until the owner says so.
- A product decision the documents do not answer is not yours to make — stop and ask.

## Report

What changed, how it was verified (commands and their result), and what is not done. The
project's check command runs on the commit you report, after the last change, and the
report quotes its final lines; "tests passed" without them is not evidence.

## Recorded provider output

A test of a provider replays a recording of the real program, never text written by hand.
When a scenario cannot be recorded on demand (a refused rate limit, a disconnected stream),
derive the fixture from a real recording by changing one field, and name the file and its
first line as derived with that one change.

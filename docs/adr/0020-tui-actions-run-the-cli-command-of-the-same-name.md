# 0020. TUI actions run the CLI command of the same name

- **Status:** accepted
- **Date:** 2026-09-24

## Context

ADR 0017 left the interface with an outbox of `Action`s and no one to carry
them out. T152 makes every action of docs/CONTRACT.md §4 reachable and
dispatched, "each calling the same core operation as its CLI command". The
operations are not in `ktask-core`: `ack`, `cancel`, `rerun-gate`, `resume`
and `retry` are `pub(crate)` in `ktask-cli`, which depends on `ktask-tui` and
not the other way round, and they print to stdout and stderr, which would
corrupt the alternate screen if called in the interface's process. Two of them
(`retry`, `resume`) run for as long as an attempt does, and a run started by
the interface must not die when the operator closes it (VISION.md §13:
closing the TUI never disturbs a run).

## Decision

The dispatcher (`ktask_tui::actions::Dispatcher`) carries an action out by
starting `ktask-rs` itself with the command `Action::command` names and the
flags the contract gives it (`--project <root> retry --task 3`), through a
`Launch` trait. The real launcher runs the current binary as a child process
in its own process group, with no stdin and stdout and stderr redirected to
unlinked files. Once a turn the shell asks the dispatcher to start what the
outbox holds and to report what has finished; the last line of the child's
stdout (on success) or stderr (on failure) becomes `App::notice`. An action
identical to one still running is not started twice.

The two view operations (`ViewOp::Attach`, `ViewOp::OpenDiff`) change only
what is shown and are applied by `actions::apply_view`, with no process.

The queue screen offers each operation from the state the journal has put the
task in, read from the inbox's record of every task's state (which follows the
core's transition table), falling back to the row's own state for a task no
event has named. The rows' own state column is not updated by any journal
event today; that is not changed here.

## Alternatives considered

- **Call the CLI's functions in-process.** Impossible without moving them into
  `ktask-core` (a dependency cycle otherwise); moving `retry`, `resume`,
  `run`'s recovery and progress pump, `cancel`, `ack` and `rerun-gate`
  wholesale is a large refactor of the command line with its own risk, and
  they would still print to the screen the interface owns. A run started in
  the interface's process would also end when the interface does.
- **Move only the small operations (`ack`, `cancel`, `rerun-gate`) into core
  and shell out for the long ones.** Two mechanisms for one rule, and the
  moved code would still need the CLI's precondition checks and messages
  duplicated or moved with it. Rejected for the sake of a single path.
- **Pipes for the child's output.** A pipe with no reader (the interface
  closed) makes the child's next write fail, and `render::progress` panics on
  a broken pipe; that would disturb the run. Unlinked files cannot.

## Consequences

Equivalence is by construction: an action can only do what its command does,
with its exit codes and its messages, and rule 2 of the contract (no behavior
in a frontend) holds. `ktask-tui` needs no new dependency. The interface
depends on finding its own executable (`std::env::current_exe`) and on the
project being addressable by `--project`; a binary renamed or deleted under a
running interface makes an action report that it could not start. An action's
result reaches the operator as one line, and only on the screens that show a
notice (queue, failures, inspector, inbox). The child runs unsupervised by the
interface: closing it neither stops nor waits for it, which is intended.

# 0017. Queue actions are recorded in an outbox on the app state

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T140 lets the operator pause, interrupt, retry and cancel from the queue
screen, each doing what its CLI command does (docs/CONTRACT.md §0 rule 1, rule
2). `update(state, event) -> state` is pure by contract (§5): it cannot send a
control request or start a retry itself. The CLI's implementations of these
commands are `pub(crate)` in `ktask-cli`, which depends on `ktask-tui`, not the
other way round.

## Decision

`App` gains two fields. `outbox: Vec<Action>` holds the actions the operator
has asked for and the shell has not yet carried out; `App::take_outbox` drains
it. `notice: Option<String>` holds a one-line message, used to say why an
action did nothing. The queue screen decides whether an action is available
from the selected task's state, by the rules the CLI commands apply, and
either appends the [`Action`](../../crates/ktask-tui/src/types.rs) or sets the
notice. A test drives every key through the headless harness and reads the
outbox, so what is dispatched is asserted without a terminal.

The action is the unit of equivalence: `Action::command()` names the CLI
command, and the shell's job is to perform the core operation that command
performs. That wiring is not part of this decision.

## Alternatives considered

- **A dispatcher passed to `update`.** Rejected: it changes the signature
  every screen and the property tests use, and makes the pure function take a
  handle to the outside world.
- **Performing the operation from the queue screen.** Rejected: it puts I/O
  in the decision logic and could not run under the test backend.
- **Returning `(App, Vec<Action>)` from `update`.** Rejected for the same
  signature churn; an outbox in the state gives the same information and keeps
  scripted sequences and snapshots unchanged.

## Consequences

`App` is larger by two fields and still `Clone + PartialEq + Eq`. Until the
shell drains `outbox` and performs the actions, they accumulate there and have
no effect on a run; the next task that wires the shell must also give
`cancel` of a task nothing is running the journal write the CLI does, which
today lives in `ktask-cli`.

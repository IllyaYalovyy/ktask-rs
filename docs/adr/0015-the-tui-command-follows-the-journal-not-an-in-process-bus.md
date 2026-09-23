# 0015. The `tui` command follows the journal, not an in-process bus

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T131 wires `ktask-rs tui` to `ktask_tui::terminal::run`, which draws state
and drains a `Receiver<Event>`. `ktask_core::Bus` is an in-process broadcast:
only a `Recorder` in the same process as the run publishes to it. `tui` is its
own process, usually started while a separate `ktask-rs run` is working, so a
`Bus` subscription in it would never see an event.

## Decision

`cmd::tui` loads the current state by polling a `ktask_tui::JournalTail` once
before the terminal is touched, then hands the tail to a follower thread that
polls it every 100 ms and sends each new event into the channel `run` drains.
The thread stops when `run` returns. The command only reads the journal, so
closing the interface never disturbs a run. It adds no dependency beyond the
path dependency on `ktask-tui`.

## Alternatives considered

- **Subscribe to a `Bus`.** Rejected: nothing publishes to a bus in this
  process, so the interface would show history and never update.
- **Let `ktask-tui` own the tail thread.** Rejected for this task, which is
  scoped to the CLI; the shell's `run(app, rx)` signature already takes the
  channel this design feeds.

## Consequences

The interface lags a run by at most one polling interval. The initial size
handed to `App` is a placeholder (80x24): the shell draws into the real
terminal regardless, but no first `Resize` event corrects `App::size`; that
belongs to `ktask-tui`.

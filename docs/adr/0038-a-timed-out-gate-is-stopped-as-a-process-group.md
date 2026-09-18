# 0038. A timed-out gate is stopped as a process group, not as a process

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T042 sets one outcome: a timed-out gate leaves no surviving children. That is a
stronger demand than "the gate's process is killed", and the gap is the whole
task. A gate is a command, and a command is frequently a runner — `cargo`, a
test harness, a shell that started a compiler server — so the interesting case
is not the process `Command::spawn` returned but everything it left running:
the lock file still held, the port still bound, the fixture still half-written,
inherited by whoever runs this gate next.

`docs/DESIGN.md:42` settles where a syscall may come from. `unsafe_code` is
`forbid` at the workspace level, which no `allow` can lift, and std has no safe
API for signalling a process group: `Child::kill` is `SIGKILL` to one pid, and
`process_group` only arranges the group, never signals it. Rather than carve out
an audited unsafe module, `DESIGN.md` sends syscalls through `nix` and
`signal-hook`; `nix` is already a workspace dependency, held in reserve for
exactly this. Adopting it in a crate is a design decision and is recorded here.

Measured while writing this, because the shape of the fix depends on the
answers:

- A background job of a non-interactive `/bin/sh` stays in the shell's process
  group, so one group signal reaches it. `killpg` still succeeds after the group
  leader has been spawned, exited *and reaped*, and it reaches the survivor —
  the group outlives its leader, which is what makes a group the right handle.
- A signal is not synchronous. A test that asserts a process is gone a
  microsecond after `killpg` returns passes against an implementation that never
  signalled anything. Every "is it gone" assertion here waits, and the one
  assertion that a helper was *left alone* waits too, for the opposite reason.
- A descendant that calls `setsid` leaves the group and is beyond reach by
  definition. It can still hold an inherited pipe open, so an unbounded wait
  after the kill would hand a stranger the length of the run.
- `CommandExt::process_group(0)` acts inside `fork`/`exec`, before the child
  runs an instruction. `setpgid` after a spawn leaves a window in which the gate
  is still in this process's group and a timeout cannot be aimed at it.

## Decision

`ktask-core` takes `nix` with `workspace = true`, and `Cargo.lock` moves in the
same commit. The crate uses only its `signal` feature; `fs` and `process` are
declared at the workspace for the later tasks `DESIGN.md` reserves them for.

- **Own group, arranged at spawn.** `own_process_group` calls
  `CommandExt::process_group(0)` on every gate. Safe, no `unsafe`, and it makes
  the gate the leader of a group its children join by inheritance — so
  `killpg` covers the tree without this process keeping a list of pids it can
  neither trust nor re-query.
- **Two steps: ask, then stop.** `Watch::enforce_budget` sends `SIGTERM` to the
  group when the budget runs out, and `SIGKILL` once `TERM_GRACE` (two seconds)
  has passed. SIGTERM first because a gate that cleans up after itself leaves no
  mess for the next run; SIGKILL after a bound because a budget that can be
  ignored is not a budget, and two seconds is a bound rather than an invitation.
- **The ladder runs on the clock, not on the exit status.** The gate's death is
  not the group's death: a descendant that deafens itself to SIGTERM keeps the
  group alive after the leader is gone, and the group is what this supervisor is
  answerable for.
- **The group is stopped once more, unconditionally, before the timeout is
  reported.** `Watch::ensure_group_stopped` fires a final `SIGKILL` when
  `timed_out` is set and nothing is being waited for any more. The escalation
  above only happens while the run is still waiting on something, and a child
  that writes nothing and heeds no signals is waited for by nobody. Gates that
  finished on their own are left alone: nothing gives the supervisor leave to
  signal the group of a gate that passed.
- **`ESRCH` is the outcome the signal was sent for.** An emptied group is not a
  failure to report; a supervisor that errored because it could not kill
  something already dead would be reporting on its own bookkeeping. A real
  refusal still travels as `Error::Gate`.
- **Listening stays bounded.** `Response` carries when each step was taken, so
  the pipes are given `TERM_GRACE + POST_KILL_GRACE` and no more. A stranger
  that left the group cannot extend either.

## Alternatives considered

- **`Child::kill()` and nothing else** — what the code did before. It is one
  `SIGKILL` at the direct child, which is precisely the leak: the grandchild
  survives, and the run reports itself finished on top of it.
- **An `unsafe` block around `libc::killpg`** — the shortest diff, and
  unavailable: `forbid` cannot be lifted at the crate, module or line, and
  `DESIGN.md` decided where unsafe lives long before this task needed a syscall.
- **Wrapping the command in the `setsid` binary** — reaches the same group
  arrangement through a subprocess that need not exist, is not present
  everywhere, and gives this process no pid it can prove it signalled.
- **Walking `/proc` for the descendant tree and killing pids** — needs a list
  that is stale the moment it is read, can race a pid reuse, and kills things
  that left the group on purpose. The group is the kernel's own answer to "what
  did this process start".
- **`SIGKILL` immediately** — costs the cleanup a test harness or build tool
  would otherwise do, and an exclusive lock left held fails the *next* task for
  a reason unrelated to it.
- **`SIGTERM` only, no escalation** — a gate that traps and ignores TERM runs
  forever, and the timeout becomes advisory.

## Consequences

- A timed-out gate's whole tree is gone before the run reports itself finished,
  which is the property `VISION.md`'s "nothing is done on an agent's say-so"
  ultimately rests on: a gate that is still running is a gate whose verdict is
  not final.
- The gate is no longer in the supervisor's own process group, so a terminal's
  Ctrl-C no longer reaches it as a side effect of sharing the foreground. A
  gate that must die with the supervisor has to be told explicitly;
  `signal-hook`, already in the fixed dependency set for this, is the future
  owner of SIGINT forwarding. Until that task, a gate is bounded by its
  timeout — not by the terminal.
- `gate.rs` is Unix-only in practice, because `nix` is an unconditional
  dependency and the `#[cfg(unix)]` pair on `terminating_signal` predates it.
  Reported as a finding rather than quietly rearranged here.
- Assertions about processes assert timing as well as outcome, so the new tests
  carry explicit bands. One of them leaks a `setsid sleep` on purpose: it is the
  only way to prove that an unreachable stranger cannot hold the run open, and
  it is unreachable by construction.
- Revisit if a gate ever needs to outlive a supervisor's timeout deliberately —
  a detached, *expected* helper would want a recorded pid and a named stop
  step, not a group signal it escapes by `setsid`.

# 0011. Run control through marker files in the state directory

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T119 asks for `pause`, `interrupt` and `cancel`, usable from any terminal
against a run in progress, signalled "through a control file in the project
state directory that the runner checks between phases and between output
chunks". The supervisor is another process, so the commands cannot act on it
directly; and the loop that reads a provider's output (`run_streaming`) has
no handle on the `Runner` — `Provider::invoke` takes only an `Invocation` and
a bus (ADR 0004 rejected widening it).

Three things had to be decided: the form of the "control file", how the
output loop learns of it, and what `pause` and a cancelled `Failed` task do
to the state machine.

## Decision

**One marker file per request** in `<state_dir>/control/`: `pause`,
`interrupt`, `cancel-<task>`. The commands create them (`ktask_core::send`);
the supervisor consumes them (`Runner`, after journaling what they asked
for). A marker carries no content.

**The output loop watches the markers through a thread-local.** Provider
invocations are synchronous, so `run_streaming` runs on the thread that
called `Runner::run_phase`. Around each `provider.invoke`, the runner
registers the `interrupt` and `cancel-<task>` paths in a thread-local
(`control::watch`, a guard that clears it on drop); `run_streaming` checks
them on the same 20 ms tick it already uses for `SIGINT` and the timeouts,
and kills the process group by the same route. No trait or signature
changes.

**Requests are discarded when a run starts.** A marker belongs to the
supervisor that was running when it was sent. `run_queue` clears the
directory first, so a request that outlived its supervisor cannot stop the
next run before it starts.

**`pause` parks the next task as `PauseReason::Blocked`.** `Paused` is a
per-task event and the state machine has no queue-level pause. When the task
in flight finishes, the runner journals `Paused { Blocked }` for the task that
would have run next and returns `RunOutcome::Interrupted` (exit 130, the
resumable pause). `Blocked` had no producer; nothing else means "a person
asked". `run_queue` puts a `Blocked` task back (`Resumed`) when it next
runs, which is how `run` and `resume` lift the pause. A pause sent during the
last task finds nothing to park and the queue drains.

**`Failed` accepts `TaskCancelled`.** A failed task holds the queue, and
cancelling it is how a human moves past it without retrying. `state::apply`
already accepted `TaskCancelled` from every state that can be waiting or in
flight except `Publishing` — publication is a commit and a push that must
end verified or not at all — and `Failed`, which this adds.

**`cancel` acts on the journal itself for a task nothing is running**
(`Queued`, `Paused`, `Failed`), and asks the supervisor for one that is
running. A running task whose supervisor has exited (its recorded pid is
gone) is refused, exit 2: `resume` reconciles it first.

## Alternatives considered

- **A single control file with one line per request.** Closer to the task's
  wording, but consuming a request is then read-modify-write, and a request
  sent while another is consumed can be lost. A marker per request makes
  send and consume each one atomic filesystem operation and needs no parser.
- **A process-wide flag, as `SIGINT` uses.** A supervisor-wide "stop now" has
  to be per task (a cancel names its task) and per project, and a global that
  a test can set poisons every other test sharing the process, which
  `scripts/quality.sh` runs under `cargo test`.
- **Pass the check through `Provider::invoke`.** Rejected in ADR 0004 for the
  same reason as it would be here.
- **Pause the task in flight at its next phase boundary.** The contract says
  `pause` stops the queue "after the current task reaches a safe boundary";
  the task boundary is the one that needs no resume point, and nothing in the
  state machine can resume a task into the middle of an attempt.
- **Scope a marker to a supervisor's pid** so stale ones are ignored rather
  than cleared. Preflight has no recorded pid yet, so the commands could not
  address a supervisor in that window.

## Consequences

An `interrupt` or `cancel` during the completion gates or publication is not
acted on until the next boundary: only the provider's invocation is
interruptible mid-flight (ADR 0004's consequence, unchanged). `cancel`
refuses a `Publishing` task; an interrupt sent then takes effect between
tasks. The commands wait up to 30 s for the supervisor and report a request
still pending after that.

A task parked by `interrupt` is `Paused { Interrupted }`. `resume` reconciles
it (`MarkInterrupted`) and does not continue it: no transition takes such a
task back to `Queued`, and this task did not add one. `cancel` is the way to
move the queue past it today.

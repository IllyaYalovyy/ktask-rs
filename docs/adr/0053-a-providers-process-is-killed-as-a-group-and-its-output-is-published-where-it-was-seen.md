# A provider's process is killed as a group, and its output is published where it was read

- Status: accepted
- Date: 2026-09-19
- Task: T057

## Context

VISION.md §12 puts a coding-agent CLI behind an adapter, and `docs/CONTRACT.md`
§4 makes that CLI's streaming output the primary thing an operator watches:
"output streams as it is produced", "arrives in bounded chunks", and no volume of
it may be buffered without limit. §12 also makes a hung session a response the
`dummy` adapter can script, which promises that something sits on the process and
ends one — VISION.md §16 ranks "hung agent" first among the operational risks,
and T058 is the watchdog written against it.

T057 is that something: `provider::process::run_streaming`, which spawns the
command, hands it its prompt, carries what it prints to whoever is watching, and
answers an `Outcome`. Five of its requirements have no precedent in this
repository, and each is a choice between alternatives that cost differently.

- **Two clocks, and one of them moves.** `gate::run_gate` has one budget in
  seconds and SIGTERMs the group when it is spent (ADR-0038). A provider has an
  idle timeout *and* a hard ceiling, and an idle timeout is a different kind of
  fact: it is a rule about gaps, so it is re-armed by the very thing it counts.
- **Publishing, not capturing.** A gate's chunks are captured and published
  nowhere, because the catalog had no entry that could carry them (ADR-0037). A
  provider's output has one — `EventKind::AgentOutput { attempt, stream, text }`
  — so this is the first code in the core that writes to a bus while work is in
  flight, from two threads at once.
- **The `attempt` field.** `run_streaming`'s parameters are a command, a prompt,
  two durations and a bus. None names an attempt, and neither does `Invocation` —
  ADR-0052 found that same absence one level up and answered it by refusing to
  invent an identity rather than by choosing a plausible one.
- **An answer for a session that never answered.** `Outcome::exit_code` is an
  `i32`, and a session killed by a clock has no exit status: `wait` reported
  nothing, and the only exited status a kill can produce is zero.
- **An output pane that must not be lied to.** A timeout is exactly the moment an
  operator most wants the last line, and it is also the moment a session is most
  likely to have printed without stopping.

## Decision

**Spawn the child as the leader of its own process group and signal the group,
never the pid.** `Command::process_group(0)` is set before the child runs one
instruction, so there is no window in which it is still in this process's group
and therefore no window in which a timeout cannot reach it; its descendants join
the group by inheritance, so one signal covers everything the session spawned
without this process keeping a list of pids it can neither trust nor re-query.
SIGTERM, then SIGKILL once a ten-second grace has passed, then a wait for the
group's `ESRCH` so that "nothing of that session is alive" is a returned fact
rather than a hope. A group that has emptied answers `ESRCH`, and that is read as
the outcome the signal was sent for — the same reading ADR-0038 records, and the
reason the guarantee costs one syscall on the ordinary path. A descendant that
called `setsid` left the group by definition and nothing here can reach it, so
the wait is bounded at two seconds past the kill: a supervisor blocked on a
process it does not own has stopped supervising the run it was keeping. The grace is five times the two seconds ADR-0038
gives a gate, and the difference is in what is being stopped: a gate is a test
suite that either halts or does not, while a coding-agent CLI is a process with a
session transcript to flush and a working tree it may be mid-edit in, so it is
given longer to answer the first signal — and the bounded wait past the kill is
what keeps that generosity from becoming the supervisor's own hang.

**An answer names the signal that actually stopped the group.** A final SIGKILL is
aimed at the group only while it still has a member: an empty group *is* the
guarantee this task is stated as, and killing nothing while reporting a killing
would make every timeout say the CLI ignored being asked to stop. Whether a
session came down on SIGTERM or had to be killed is the difference between a CLI
that behaves and one that resists being stopped — a distinction an operator reads
and a failure classifier acts on — so it is kept rather than smoothed into one
word for "timed out", and the label is raised only by a signal aimed at something
that was there to receive it.

**Two clocks, both absolute, and the idle one is re-armed by every chunk.** The
hard deadline is fixed at the spawn and bounds the session however productive it
proves. The idle deadline is *last chunk plus `idle_timeout`*, recomputed in the
collector as each chunk is observed, and capped at the hard deadline so output
can buy a session time but never immunity. Re-armed rather than extended by a
step: VISION.md §7's taxonomy distinguishes an agent that is slow from an agent
that is gone, and a session printing once a second is the first — T058's
surviving case — while a session that prints once and then sleeps is the second,
its silence the number a hang is classified from. Re-arming in the collector
rather than in whichever reader saw the chunk is what stops one stream's drain
rate from deciding how long the session lives.

**Publish from the reader thread, at the instant the line was read.** Each reader
stamps its own `Stream` and hands the event to the bus under the bus's own lock.
The alternative was `run_gate_streaming`'s shape — both readers into one channel,
one collector forwarding to a callback — which is right for a callback that must
be called from one thread without a lock and wrong here twice over: forwarding a
chunk before stamping it makes the arrival order a property of when somebody
emptied the queue rather than of when the bytes were seen, and a chunk still in
that queue when a hard timeout fires is a chunk that never reaches a screen. A
line is the chunk, because `AgentOutput` is defined as one line (ADR-0052) and
because half a multi-byte character is neither renderable nor attributable;
carriage returns, control characters and over-long lines stay the renderer's
problem, as `docs/CONTRACT.md` §4 rules.

**An attempt this function was never told stays untold, and an unattributed
session publishes nothing.** `pub(crate) run_streaming_as` takes the
`Option<AttemptId>` the required signature cannot, `run_streaming` is that
function with `None`, and a line with nowhere honest to attribute goes to the
capture and not to the bus. `AttemptId::new(0)` was the alternative, and it is
the substitution ADR-0049 refuses for a token count — an id no runner ever handed
out, indistinguishable afterwards from a real attempt, in the one stream the TUI's
Live screen will filter by attempt and `state.rs` already *refuses* an output
event whose attempt is not the active one. `task_id` is `None` for the same
reason. This is a gap in the design rather than in this file, and it is reported
rather than filled: `Provider::invoke` hands an adapter an `Invocation` and a
`Bus`, and neither carries the attempt, so either `Invocation` grows one or the
runner publishes the output it starts. T057 leaves both open and names
`run_streaming_as` as the door either of them walks through.

**A session this supervisor stopped is an error, never an `Outcome`.** ADR-0050
considered `exit_code: Option<i32>` and rejected it, assigning "killed by a
signal rather than exited" to this task and to the error channel; that settles
both clocks and any session ended by a signal nobody here sent. The idle error
names the silence it measured as well as the budget it exceeded, because "the
idle timeout expired" is a fact about a timer and the silence is the fact about
the agent. What was read before the stop is in the error's detail: how much of
each stream, what was dropped, and — if the CLI refused the prompt — what it
answered when written to, since a session that never read its prompt is one
common reason a session looks silent.

**The prompt is written on a thread of its own, and a refusal to take it is the
CLI's answer, not this function's failure.** A CLI that never reads its prompt
keeps the pipe's buffer, and a blocking write from the calling thread would sit
inside the session with both clocks running; on the way out a thread still
holding the write end would keep it open for the child forever, so the join
happens before the last word is said. `EPIPE` is what a CLI that takes its prompt
as an argument does to a prompt on stdin — Claude and Codex both (T059, T060) —
so refusing the session for it would fail every real adapter, and inventing a
field to say "it did not read the prompt" answers a question no caller acts on
when the session went on to succeed. `MAX_PROMPT_BYTES` (4 MiB) is refused before
the spawn: a prompt that large cannot be written into a pipe nobody is draining,
and a session that never started has nothing to clean up.

**Each stream is kept whole up to a megabyte, and what falls off is the end.**
A gate's timeout may discard everything past its first 64 KiB; a timeout's output
is the evidence a failure is classified from, and a session killed for printing
without stopping is the one that produces the most. Lines are therefore dropped
only once a stream has passed the bound, and each dropped line is counted, so
what remains is a true prefix that can be quoted from rather than a window that
skipped. Deliberately left undone, in the shape of ADR-0052's last section:
**`Outcome` has no field for "this capture is short"**, so a run that finished by
itself past a megabyte of one stream reports a truncation nobody can see from its
answer — the field that names it belongs to the attempt record (T068), and a
timeout does report it, because that error has a detail.

## Consequences

- A provider's CLI can be killed without killing this process's own shell, and
  everything it spawned goes down with it; a helper that detaches with `setsid`
  is out of reach by definition and the bounded wait is what stops the supervisor
  being the thing that hangs instead.
- A timeout's detail says which signal stopped the group, so "it had to be
  killed" and "it came down when asked" are two different answers, and the first
  one is only ever given about a group that was still there to be killed.
- A subscriber sees provider output while the session runs, on both streams, in
  the order it was read — and sees nothing from a session nobody attributed.
  `gate::run_gate` still publishes nothing (ADR-0037), so the two streaming paths
  differ, and the difference is the catalog's, not an oversight.
- The idle timeout now *means* "no output for this long", so a session that
  prints forever slowly outruns only its hard ceiling. Anyone who wants a ceiling
  on output rather than on silence asks for the hard one.
- Both timeouts are `Err`, so a caller cannot mistake a stopped session for a
  session that finished; it also means a stopped session's partial capture is
  reachable only through that error's detail and the bus, not through an
  `Outcome`. T058's watchdog is written for exactly that shape.
- `run_streaming` returns `Ok` only for a session that returned an exit code of
  its own, so an adapter that wants the signal a CLI died on reads it from the
  error's detail.

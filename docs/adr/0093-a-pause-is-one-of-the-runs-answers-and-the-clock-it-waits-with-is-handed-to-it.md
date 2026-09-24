# 0093. A pause is one of the run's answers, and the clock it waits with is handed to it

- **Status:** accepted
- **Date:** 2026-09-23

## Context

`docs/CONTRACT.md` §1 gives three of its seven exit codes to stopping without
failing: 3 a provider limit, 4 a human gate, 5 a decision an agent put to a
person. Its own words: "Codes 3, 4 and 5 are pauses. They are not failures and
must never mark a task `failed`." VISION.md §6 turns the same three into durable
states — `waiting_limit`, `waiting_input`, `human_gate` — and §3's eighth
invariant makes the second the mechanism behind "nothing is done on an agent's
say-so".

Before this task none of the three was an outcome a run could produce.

- **A limit** was recognised (`limit_message`, T065) and ADR-0061 finished the
  arithmetic around it (`parse_reset`, `wait_plan`), then wrote in its own
  Consequences: "Nothing sleeps yet. `WaitPlan` is not wired to `PauseReason` or
  to a scheduler, so in practice a limit is still waited out by its caller's
  backoff." Nothing in the workspace called either function.
- **A `NEEDS_INPUT` report** reached `never_looped` and came back as an
  [`Error`], which whatever receives it can only read as a fault.
- **A `**Gate:**` task** was a task like any other: prepared, locked, checked
  out, and handed to a session — which is exactly what VISION.md §6 forbids.

Six questions had to be settled where the documents state a requirement rather
than a mechanism.

**How a run can be tested against an instant it intends to sleep until.** A test
that slept for a twenty-minute reset would take twenty minutes; a test that did
not sleep would not have tested the wait, and would have proved the *fake*
behaviour rather than the real one.

**Where the pause row goes relative to the sleep.** §3's third invariant journals
every transition before its side effect, and ADR-0009's whole reason for adopting
`time` was that the journal holds the instant a restarted run wakes at.

**Which row a question writes.** [`EventKind::Paused { reason: Input }`] and
[`EventKind::DecisionRaised { request }`] move a running task to the *same*
paused state, and ADR-0026 makes [`apply`] refuse a second pause below a task
that is already paused. ADR-0079 wants the row to carry the question itself.

**What a gate may not do while a person decides.** §10 makes the repository lock
the thing the rest of the queue waits behind.

**Whether a wait is remediation.** §7 bounds *remediation* by attempts, elapsed
time and tokens; a provider's "come back at 12:20" is neither a repair nor a
failure.

**Where §7's account goes when the answer is a pause.** ADR-0091's
[`EventKind::SelfHealingReport`] is admitted by [`crate::TaskState::Remediating`]
and by no state after it, so an account written after the pause row would
corrupt the projection.

## Decision

**Three refusals answer with a state, not an error.** `Runner::answer_the_refusal`
now returns `Answer`, which is either `Repair(Remediation)` — another attempt —
or `Parked(TaskState)` — the run stops here, at the state its own journal already
reaches. Naming both in one enum is what keeps a pause from being spelled as a
refusal: an [`Error`] out of that step is a fault of the run's own, and §1 and §3
both insist that a limit, a gate and a question are not faults.
`run_task`/`run_task_with` still return `Result<TaskState>`: a pause arrives as
`Ok(TaskState::Paused { .. })`, which is a resumable state and not a verdict.
The `RunOutcome` the CLI will map to exit codes belongs to the task that owns
that enum.

**The clock is a parameter of the run, not a sixth field of it.** `Clock` asks
two questions — `now`, and `sit_out(plan)` ("sit this out, and tell me whether you
actually waited") — and is handed to `run_task_with` beside the environment
closure that already reaches the run for the same reason: an instant a run sleeps
to cannot be supplied by a test that has to sleep for it. `Machine` is the
production answer, the real instant and a thread that really wakes. It is a
parameter rather than a field because the instant a pause is *planned* against and
the instant it is slept *to* have to be the one instant the caller decided, and
because a [`Runner`] is five things: a field nobody can swap without building a
run is a field a test cannot answer for.

**A limit is journalled, then slept, then closed.** In this order: §7's account,
`now` from the clock, `parse_reset` over what the session printed, `wait_plan`
with this project's own margin and ceiling, the `Paused { Limit { until } }` row,
and only then the sleep, closed by `Resumed`. `until` is `None` when the provider
named no instant — the pause state's honest version, not an invented one. Because
the row precedes the sleep, a process killed mid-wake resumes the wait it promised
rather than a shorter one.

**One wait per run, and a wait spends none of §7's bounds.** A second limit in
one run is the provider contradicting the reset the first wait was planned around,
which is a fact about the provider a waiting supervisor cannot settle and a screen
can, so `Budget` allows one sat-out wait per run. A wait is charged to nothing:
not an attempt, not elapsed time, not tokens. A limit met where the bounds are
already spent still parks, and parks with the same deadline a run that could wait
would have written, because a spent budget is not a different limit.

**A question writes the row its own shape supports.** A report whose ask fills
[`crate::decision_request`]'s four labels journals `DecisionRaised`, *which is
itself the pause row*; an ask short of one of them, or a session that only printed
its question, journals `Paused { reason: Input }`. Either way the task ends in the
same state with the same `resume_to`; the shortage costs the structured request
and not the pause. Nothing is invented to fill a shape the agent did not write.

**A gate is parked before anything is started, and parking is idempotent.** A task
with a `**Gate:**` section never reaches `Runner::prepare`: one `Paused { reason:
HumanGate }` row, and no preflight, lock, checkout or session behind it, because a
gate that had taken the lock would hold it for the length of a human's decision.
The step reads the state its own rows already reach first, so a second run over
the same gate answers with the pause it finds instead of writing a nested row
ADR-0026 would refuse.

## Alternatives considered

- **`Clock` as a `Runner` field, or `OffsetDateTime::now_utc()` at the call
  site.** The field version belongs to whichever step happens to ask, and the
  inline version cannot be tested against a decided instant at all — the same
  argument ADR-0061 made about `parse_reset` reading the clock itself.
- **Sleeping inside `wait_plan`.** ADR-0061 refused it: a value can be journalled
  and a sleep cannot. This task is the caller that ADR-0061 left unmade, not a
  reopening of its decision.
- **`Err(Error::Paused)`, or returning `RunOutcome` from `run_task`.** A
  `Result::Err` is a fault, and mapping work belongs to T105. `TaskState::Paused`
  inside `Ok` is already the state a resume continues from, so the pause needs no
  new type here — only a channel that is not the failure one.
- **Charging the wait to the attempt budget.** A limit naming a twenty-minute
  reset would end the task as `TaskFailed` at an elapsed bound its own provider
  refusal had nothing to do with, which is §1's "must never mark a task failed"
  broken by a bookkeeping choice.
- **Jitter in the sleeper.** VISION.md §7 lists it beside the margin, and
  ADR-0061 left the sleeper owing it. It is still absent, recorded rather than
  denied: a jittered wait would need a random source and a decision about its
  distribution, neither of which T097 asks for, and the exact instant the row
  holds is the thing recovery is tested against.
- **Writing `Paused { Input }` after `DecisionRaised`.** The fold refuses a second
  pause below a paused task, and the row the state needs is the one that carries
  the question anyway.
- **Parking a gate after the preflight.** One fewer special case in the runner, and
  the queue stalled behind a lock held for however long a person takes.
- **A background sleeper thread that lets the run return at once.** The `Paused`
  row would then describe a wait owned by a thread no journal mentions, and the
  "is this run still waiting" question would have two answers. Whether the caller
  waits or returns is `sit_out`'s to answer, which is one decision in one place.

## Consequences

- `run_task_with` has a fourth parameter, so every test module that drives a task
  names a clock; the ones that do not care about time pass `&Machine`.
- A caller that must answer now installs a clock whose `sit_out` says no, and gets
  the pause with its deadline still owed. Which clock `ktask-rs run` installs is
  the decision T105 makes when it maps outcomes to codes, and T124 asserts the
  three exit codes end to end.
- Until that choice is made, `run_task` waits in the foreground: a limit naming a
  reset twelve hours ahead sleeps twelve hours, bounded by
  `limit_max_wait_secs`, a day by default. That is §7's "waits until the
  exact reset time" read literally, and it is a screen-visible change from a run
  that used to fail fast.
- Jitter exists nowhere in the workspace still, so §7's list stays half-implemented
  and ADR-0061's deviation stays open. `Machine` sleeps to the instant it was
  given, and no two runs of one project wake apart.
- A `FailureClass::ProviderConfiguration` refusal still comes back as an [`Error`]
  with no row, so its task is left where the attempt left it rather than in a
  pause state. §1 names no exit code for it and §6's `blocked` is the state that
  would hold it; choosing between them is the mapping task's, not this one's.
- The one-wait-per-run ceiling is `LIMIT_WAITS_PER_RUN`, not configuration:
  `docs/DESIGN.md` names `limit_wait_margin_secs` and `limit_max_wait_secs` and no
  third knob, and a knob nobody has asked for is a key with one test behind it.
  It becomes a key the day a second consumer wants a different number.

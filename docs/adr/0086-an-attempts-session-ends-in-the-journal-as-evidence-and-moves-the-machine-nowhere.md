# 0086. An attempt's session ends in the journal as evidence, and moves the machine nowhere

- **Status:** accepted
- **Date:** 2026-09-22

## Context

T091 is the task that starts a provider session and reads what it left, and
`docs/DESIGN.md` gives the moment its own catalog entry: `AttemptFinished`, with
`attempt`, `exit_code`, `usage`, `session_id` and `model_reported`. ADR-0011 kept
that entry out of `EventKind` until a task emitted it, on the rule that an entry
whose effect on state nobody has written is a row the journal would accept and
`apply` would have to answer with a guess. This is that task, so the entry and
its `apply` arm land together — and the arm has to be written before the row can
be, which forces four questions the task's one-line sketch does not answer.

**What does an entry named *Finished* do to the state machine?** Its
`exit_code` invites the obvious answer — the session ended, so the attempt's work
is over, so this is the transition out of `running`. VISION.md §3's invariant 4
forbids exactly that reading: "no task is done on an agent's exit code or
statement". A machine that moved on a session's status is a machine whose
completion is the agent's say-so in a different font, and §9 puts the way out of
a phase in the phase's gate.

**Which states may answer it at all?** The row names an attempt, so a state
holding none cannot attribute it. That leaves seven states holding an attempt
number, and they are not alike: in `running` and `remediating` an agent is at
work and a session is open to end; in `verifying` and `publishing` the sessions
are over and mechanical checks are what run; `published_verified`, `failed`-bound
and `paused` hold an attempt as a memory rather than as work in progress.
`AgentOutput` already answers that question for one entry — it is accepted in
`running` and `remediating` only — and a session's *end* is the same kind of fact
as a session's *output*.

**Why does `Verifying` refuse it while accepting `AttemptRecorded`?** Both are
evidence rows, and the sweep that reads `LEGAL` (ADR-0025) shows the pair side by
side, so the asymmetry has to be a decision rather than an accident. `AttemptRecorded`
is the attempt's own evidence — gates run, base and candidate SHAs, its task
beside them (ADR-0063) — and it is legitimately filed when the attempt's last
phase has ended and its gates have answered. A session's account is not that: it
is one process's report of one phase, and a `verifying` state that accepted one
would accept a session nobody in this state started.

**Does `exit_code` belong in the payload at all,** once the machine is forbidden
to read it? ADR-0049's rule about unreported figures and ADR-0057's about a
reported model id both apply to the other three fields, and neither is a reason to
drop a field `docs/DESIGN.md` names.

## Decision

**`AttemptFinished` moves nothing, in every state.** `apply` answers it with the
state it was asked from, as it already answers [`EventKind::AttemptRecorded`],
[`EventKind::GateStarted`]/[`EventKind::GateFinished`] and
[`EventKind::TddExceptionUsed`]: the journal keeps the finding, the machine keeps
its own reasons for moving. The row is written *because* the session is not
reconstructible afterwards — how it stopped, what it spent, which session it was,
which model it said it ran on are five facts nobody else was holding.

**It is accepted in `Running` and `Remediating`, under attempt equality,** in the
same match arm as `AgentOutput` and `VerifyFailed`: the state that holds the
attempt the row names answers it, any other refuses. Everywhere else it is
refused — `Queued` and `Preflight` hold no attempt, `Verifying` and `Publishing`
have no session open, `PublishedVerified` holds its attempt as history, and
`Paused` has been parked, which is what [`EventKind::Interrupted`] records about a
phase in flight rather than what a session reports about its own end.

**`exit_code` stays, and is evidence only.** `an_attempt_finish_moves_nothing_and_names_the_attempt_it_ends`
asserts the move with `exit_code: 137` and again with `exit_code: 0` and gets the
same state both times, and a scenario is free to contradict its own report with
the number: that contradiction is invariant 4 being *tested* rather than merely
quoted.

**The row is written after [`check_model`], never before.** A configured id and a
reported id that differ are [`Error::Config`] and no row is appended (ADR-0057),
so what reaches the journal under `model_reported` is an id somebody confirmed or
an honest `None` — never a mismatch, and never the configured id filled in for a
session that said nothing.

**The log places it at `Info`** and its message carries the four facts that are
not already columns: the attempt it names is the line's `attempt` column and is
not repeated in prose a filter cannot use.

**The declared table gains exactly one row:**
`("Running", "AttemptFinished", "Running")` — 67 rows against the 300 pairs of
the sweep, eight exhaustive matches (ADR-0022). `Remediating` gets no row because
the sweep's remediation holds attempt 2 while its sessions all ended attempt 1, so
what the sweep can show for that state is the refusal; the moves it does make, and
the refusals equality produces, are held by the named test instead.

## Alternatives considered

- **`Running` → `Verifying` on a zero exit code.** It is the reading `exit_code`
  invites and the one §3's invariant 4 names and forbids. It lost because a phase
  that wrote outside its scope, printed nothing and exited 0 would advance.
- **Accept it in every state holding an attempt.** One fewer refusal, and the loss
  is the check itself: a row whose session cannot exist in the state asking about
  it is the shape of a forged or misplaced record, and refusing it is the only way
  the journal says so out loud.
- **Drop `exit_code` since nothing reads it.** `docs/DESIGN.md` lists it, and it is
  the difference between a session that finished and one a signal took, which is
  what a failure bundle quotes. Reading it is what is forbidden, not holding it.
- **Fold the five fields into `AttemptRecorded`.** ADR-0063 already weighed the
  overlap and kept them apart: one row per attempt, written once, refused
  contradictorily by `write_evidence` — a retry adds a session's account instead
  of rewriting an attempt's evidence, and a phase that ran two sessions leaves two
  accounts.
- **Append the row as soon as the provider returns, before checking the model.**
  A journaled row is evidence a later task reasons from; recording a session whose
  model was never confirmed puts a contradiction in the record and then refuses
  the run for it.
- **A `_ =>` arm in the eight per-state helpers.** ADR-0022 made those matches
  exhaustive precisely so a new catalog entry is a compile error everywhere rather
  than a silent self-transition in six states and a guess in two.

## Consequences

- `run_phase` now has a journal boundary: a row that ends a session and a report
  that was never read are two readable facts, so recovery can distinguish a
  session that finished from a run that died mid-session. Before this entry the
  second case looked exactly like the first.
- A session that outlives the pause that was waiting for it is refused by
  `Paused`, so the row is not silently dropped — it arrives as
  `Error::InvalidTransition` naming both halves. Which step owns killing the
  session inside `attempt_timeout_secs` is the watchdog's task, not this one's,
  and this is where that task's failure will first be visible.
- A phase that runs two sessions journals two rows on one attempt, and nothing
  reads a "total" out of them; the attempt's figures are still `AttemptRecorded`'s
  single `usage`, so nothing is double-counted by accident.
- `Remediating` accepting the row while holding no `LEGAL` row means the sweep
  alone would not catch the arm being deleted — the named test would, and the
  reason is written on the table rather than left to the reader.
- `Exit` figures are journaled and unused. If a later task wants a run to treat a
  signalled session differently, that is a new decision about invariant 4, not a
  field to start reading.

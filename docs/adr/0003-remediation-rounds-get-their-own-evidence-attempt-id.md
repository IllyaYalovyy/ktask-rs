# 0003. Remediation rounds get their own evidence attempt id, not a new `AttemptStarted`

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T096 asks `Runner` to retry a failed attempt once, from a fresh provider
session, "done-when" a dummy scenario that fails once then succeeds "reaches
Done with two attempt records."

`state.rs`'s transition table only accepts `EventKind::AttemptStarted` from
`TaskState::Preflight` (`from_running`, `from_remediating` and
`from_verifying` all list it among the events they reject). `VerifyFailed`
already moves `Verifying` to `Remediating`, seeded with whatever `attempt` id
`Verifying` already carried — it does not, and structurally cannot, mint a
new one, since none of its match arms read the event's own `attempt` field
for that transition. So a remediation round cannot call
`Runner::begin_attempt` (which journals `AttemptStarted`) without producing
an event no reachable state accepts, which `journaled_state`'s fold over
`apply` would reject with `Error::InvalidTransition`.

At the same time, `attempt::read_evidence` is keyed by directory name
(`<state_dir>/attempts/<task>/<attempt>/`), one per `AttemptId`. Reusing the
original attempt's id for a remediation round's evidence would overwrite the
same `record.json`, leaving `read_evidence` return exactly one record no
matter how many rounds ran — never the "two attempt records" the task's
done-when names, and not what `docs/CONTRACT.md`'s `retry` command ("starts
a fresh remediation attempt") or its Task inspector / History screens
("per-attempt evidence", "event timeline across every attempt and
remediation") describe either.

Separately, `log.rs`'s `attempt_of` already reads the `attempt` field out of
each event's own payload (`PhaseEntered`, `AttemptFinished`,
`VerifyPassed`/`VerifyFailed`, `PublishStarted`, `AttemptRecorded`) to decide
which attempt a log line belongs to — not `TaskState`'s own tracked
`attempt`. Nothing in `state.rs`'s transition functions cross-checks an
event's own `attempt` payload against the state's stored value either: every
arm that keeps or advances `Running`/`Remediating`/`Verifying`/`Publishing`
reuses the *state's* captured `attempt`, discarding whatever the incoming
event's payload said, with the single exception of the very first
`AttemptStarted` that establishes it.

## Decision

Each remediation round is assigned its own `AttemptId`, one past every
attempt `read_evidence` already finds on disk
(`Runner::next_evidence_attempt_id`), and that id is used in every event the
round journals (`PhaseEntered`, `AgentOutput`, `AttemptFinished`, the
`PhaseEntered{Verify}` that follows, and whatever `Runner::verify_and_publish`
itself journals for that round). `Runner::begin_attempt` — and
`AttemptStarted` — are never called again for the same task run.
`TaskState`'s own `attempt` field, established once by the original
`AttemptStarted`, is left exactly as `state.rs`'s existing transitions
already leave it: unread and unchanged by any of these events. This is safe
specifically because no transition function ever validates an event's
`attempt` payload against the state's own, as established above.

Each round's evidence is written directly with `write_evidence`, not through
`begin_attempt`: a failed round's exit reason and classification, or a
successful round's candidate commit, once the round concludes. A round's
provider `Invocation` never carries the previous round's session id forward
— `Invocation` has no field for one — and each round's own `AttemptRecord`
only ever records the session id its own `Outcome` reported.

## Alternatives considered

- **Reuse the failed attempt's own `AttemptId` for every remediation round.**
  Rejected: collapses every round's evidence into one directory, so
  `read_evidence` can never show more than the single most recent round —
  contradicts the task's own done-when and `docs/CONTRACT.md`'s per-attempt
  evidence model.
- **Call `Runner::begin_attempt` for each round, accepting a second
  `AttemptStarted`.** Rejected: `state.rs` rejects that event from every
  state but `Preflight`; accepting it would mean loosening `state.rs`'s
  transition table specifically to accommodate this task, which is a
  materially bigger change than this task's own scope (`runner.rs` alone)
  and would let *any* caller inject a spurious second attempt start into an
  in-flight run.
- **Add a new `EventKind` (e.g. `RemediationStarted`) that legitimately
  advances `TaskState`'s tracked attempt.** Rejected as beyond this task's
  file scope (`runner.rs` only); also unnecessary, since nothing downstream
  currently depends on `TaskState::Running`/`Remediating`'s `attempt` field
  matching the evidence directory in use — `log.rs` already keys off each
  event's own payload instead.

## Consequences

`ktask-rs status`'s `attempts` count and any future TUI view that wants "how
many attempts has this task taken" must derive that from evidence
(`read_evidence`) or from counting `AttemptRecorded`/journaled per-attempt
events, not from `TaskState::Running{attempt}`'s single stored id, which
only ever reflects the run's original attempt. A future task that wants
`TaskState` itself to track the currently-remediating attempt's id precisely
will need a new `EventKind` and `state.rs` transition, as the alternative
above describes; this task deliberately leaves that undone.

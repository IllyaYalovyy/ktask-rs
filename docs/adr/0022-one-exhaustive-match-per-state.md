# 0022. One exhaustive match per state

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T022 writes `apply(state, event) -> Result<TaskState>`, the single gate through
which a task's state may change (`docs/DESIGN.md`: "`apply` is the only way
state changes"; VISION.md §6: every transition is journaled before its side
effect, so an interruption resolves to a known state). Twelve states by nineteen
events is a surface of roughly 228 answers, and three forces shape how the
answers are written down.

- **A new event must not be silently ignorable.** The event catalog grows as
  tasks land — `docs/DESIGN.md` lists 28 entries and 9 are absent because their
  payload types do not exist yet — and every one of them will need a meaning in
  every state. An answer that compiles when the question is new is an answer
  that was never given.
- **The function is pure and cannot consult anything but its arguments.** No
  clock, no I/O, and no `Config`: the attempt and remediation budgets
  (`max_attempts`, `max_remediation_attempts`) live in configuration this
  function may not read.
- **The project's own lints forbid the obvious single match.** `clippy`
  runs with `-D warnings`, `too_many_lines` is 120 and `cognitive_complexity`
  is 20 (clippy.toml), and no line-level `#[allow]` is permitted (AGENTS.md).

What was tried, rather than reasoned: two experiments add a variant with no
matching arm and record what the compiler says. A `TaskState::Stalled` with no
arm fails `cargo check -p ktask-core` twice — once in `TaskState::name`, once in
`apply`:

    error[E0004]: non-exhaustive patterns: `&TaskState::Stalled` not covered

An `EventKind::Nudged` with no arm fails it nine times: once in
`EventKind::discriminant` and once in every helper. Both variants were removed
again before this was written, which is the point — the compiler, not a review,
is what decides whether the question was answered.

## Decision

`apply` is a dispatcher that matches on `state` alone and delegates to one
private function per non-terminal state — `from_queued`, `from_preflight`,
`from_running`, `from_remediating`, `from_verifying`, `from_publishing`,
`from_published_verified`, `from_paused` — each of which matches on the event.
The four terminal states are refused in `apply` itself, in one explicit
or-pattern. No helper has a wildcard arm: each names all nineteen catalog
entries, the illegal ones collected into a single arm by `|` so that they
return `Error::InvalidTransition` naming the state and the event, which is what
`TaskState::name` was written for (ADR-0021).

Six rules decide the answers, and they are stated here because the table is too
long to review as a table:

- **An event whose fact is already true, or which the state has no field to
  hold, is a self-transition rather than a refusal.** `PreflightPassed` on
  `Preflight`, `AgentOutput` on the attempt in flight, `PublishStarted` on
  `Publishing`. A refusal here would make replay of a journal an error, and
  recovery *is* a fold of the journal: a state has to accept the events that
  created it.
- **Attempts never run backwards.** An event naming an earlier attempt than the
  state's is refused; one naming a later attempt is accepted and recorded but
  not adopted, because a state names one attempt and the journal is where the
  next one is first true.
- **A failed gate is never terminal.** `VerifyFailed` leaves the task where the
  remediation starts from. Whether a further attempt is still allowed is
  configuration; the runner counts and either journals a later `AttemptStarted`
  or journals `TaskFailed`. Bounding it here would bake a limit into the one
  component that owns none.
- **Only `VerifyPassed` opens publication, and only `PublishVerified` closes
  it**, and then only when its `commit` and `remote_sha` name the same commit.
  `PublishStarted` is refused from every state that is not already publishing,
  so nothing reaches `PublishedVerified` without a passed gate behind it —
  invariant 2 in the place it is easiest to break.
- **Only `TaskDone` of the proved commit reaches `Done`.** Any other commit is
  a claim about work that was never published.
- **A pause is a wait, never a failure.** `TaskFailed` is refused from
  `Paused`; a second `Paused` first nested and is now refused — that half of
  the rule was withdrawn by ADR-0026, which T028 asked for and which settled
  what a pause's one return address is worth; and `GateAcknowledged` closes
  only a pause that stopped at a `PauseReason::HumanGate`, which is how
  VISION.md §6 says a gate reaches `acknowledged`.

## Alternatives considered

- **One match over both axes.** Rejected on the project's own terms: roughly
  twelve states by nineteen events cannot fit 120 lines or complexity 20
  without an `#[allow]`, and it would answer "what does this event mean" from a
  place that has to know every state at once.
- **A wildcard arm per helper, or `EventKind::_ =>` syntax.** Rejected outright:
  it is the one mistake this file must make impossible. A new variant would
  compile, default to refused, and reach `main` with no run of the compiler
  saying that twelve places had been asked nothing. The experiments above are
  the counterfactual, measured.
- **`apply(state, event, config)` so a failed gate can end the task.** Rejected:
  it makes the transition function depend on a value that can change between
  journaling and replay, so the same journal would fold to different states on
  two days. The budget belongs to the runner, which is the part that may read
  config and write events.
- **Refusing a redundant event rather than self-transitioning.** Rejected: it
  breaks replay, and recovery cannot be the component that has to be careful
  about ordering (VISION.md §10).
- **Making `PhaseEntry`/`phase_entry` public** so later tasks can ask which
  half of the pipeline a `Phase` sits in. Rejected for now: nothing outside this
  module has asked, and an unasked-for public type is surface the contract does
  not list.

## Consequences

- Adding an `EventKind` variant fails to build with one error per state —
  measured at nine sites, eight helpers plus `EventKind::discriminant` — and
  adding a `TaskState` variant fails to build until `apply` says where it goes.
  The cost is real: a new event is a twelve-line edit, in the honest sense that
  every state must be read. That is the trade this ADR accepts.
- Because terminals accept no event, `Done` is the only state `apply` can reach
  and `Acknowledged` is reachable only from a `Paused` at a human gate. ADR-
  0021's expectation that human-issued events "leave" these four states is
  therefore satisfied by events *arriving* at them: `ack` closes a gate, it does
  not re-close a finished task. `retry` has no event that leaves `Failed` — the
  catalog offers none — so it is a fresh task rather than a transition, which is
  what `docs/CONTRACT.md` says it is. The task that owns `retry` (T116) should
  read this before adding an event to the catalog.
- Later tasks owe this function an order. `VerifyPassed` must be journaled
  before `PublishStarted`, which must be journaled before `PublishVerified`;
  and a pause may be closed by `Resumed` *or* by `RecoveryDecision{ Resume }`,
  never both, since the second finds nothing paused.
- `TaskState::Publishing` carries only its attempt, so `apply` cannot compare
  `PublishStarted.candidate_sha` against the commit being pushed; that check
  belongs to `git::publish` and to recovery. Widening the variant is a change to
  durable data and to `docs/DESIGN.md`, not a detail to fix in passing.
- One rule stated here no longer holds: the nested second `Paused`. ADR-0026
  refused it on T028's completion check, so `from_paused` answers a second
  `Paused` with `InvalidTransition` and `a_pause_above_a_pause_is_refused`
  asserts that instead. Everything else in this record still stands.

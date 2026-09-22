# 0087. A phase's answer is what it touched beside what it claimed, and the scope check outranks the claim

- **Status:** accepted
- **Date:** 2026-09-22

## Context

`Runner::run_phase` is the first step that holds a provider session and the
run's own measurement of it in the same call. Everything before it either
decided the work could start (`prepare`) or named where the agent's account
would go (`prepare_report`, `read_report`); everything after it reads an answer.
Five forces meet in this one function, and four of them force a decision the
task's one-line sketch does not make.

**Two refusals arrive at the same moment, of two different kinds.** A phase that
wrote a path its scope did not grant is refused by
[`protocol::check_scope`], which answers with [`Error::Policy`] quoting
`SCOPE_RULE` and every offending path. A phase that ended and left no report is
answered by [`report::read_report`] with [`ReportClaim::Missing`], which already
carries a [`FailureClass`] and the path it was missing from. One is the run's own
check refusing; the other is the agent failing to account for itself. A single
return type has to express both without either one being flattened into the
other, and the task's signature fixes that type as
`Result<PhaseOutcome>`.

**Which answer wins a phase that did both?** A phase can write outside its scope
*and* stay silent, and the two checks are neighbours at the end of the function.
Nothing in VISION.md orders them, and the order is observable: it decides what a
retry sees.

**Is a missing report an error?** Turning it into one is the obvious shape — an
`Err` is how a refusal usually leaves a Rust function. But the class is not this
step's to invent: ADR-0057's rule about a reported model id generalises to any
figure the run records, and `read_report` already reports the class rather than
leaving it to be inferred. §7 makes recovery policy a decision read *from the
class*, and the two classes here sit at opposite ends of it: an agent that said
nothing is exactly what a fresh session is for, while `check_scope`'s own doc
states that the `policy_failure` it returns "earns no retry: an exception is not
a repair for what was already written". Flattening both refusals into one `Err`
is how that distinction gets lost.

**The session wants a bus this step does not have.** `Provider::invoke` takes
`Option<&Bus>` and streams each line into it as it is produced; the recorder that
writes the phase's rows owns a `Bus` and offers [`Recorder::subscribe`], not a
handle to lend. The `dummy` adapter's publishing is also *its own* copy of each
line, built before the journal existed and therefore stamped with
`EventSeq::new(0)` rather than the sequence a row is given (ADR-0016 is why the
recorded row is read back rather than trusted).

**The prompt is read from a home a test may not touch.** The context document and
this project's own template come from `paths::base`, which resolves
`XDG_CONFIG_HOME` and `HOME` from the process environment, and `docs/DESIGN.md`
Conventions keeps a test out of the process environment and out of the operator's
real configuration. A step that assembles a prompt therefore cannot be tested at
all unless the accessor can be handed to it.

## Decision

**`PhaseOutcome` has two arms, and neither is a verdict.** `Claimed` carries the
phase, the attempt, the `ReportResult` the file's header held, the whole text of
the report, and every path the checkout now reports against
`Prepared::base_sha`. `Unreported` carries the phase, the attempt, the class
[`ReportClaim::Missing`] gave, the path it was missing from, and the detail that
names that path. `changed` is measured once, before either arm is built, so the
paths a scope refusal quotes and the paths the claim arm carries are one
measurement rather than two accounts that might disagree.

**A scope violation is `Err`; a missing report is `Ok`.** The violation is the
run refusing, and [`Error::Policy`] is the shape [`classify`] already lands as
[`FailureClass::PolicyFailure`] — the class whose own doc says it earns no
retry, which is the correct verdict for a file that is already written. The
missing report is the agent's failure, and it comes back as data so the class
VISION.md §7's recovery policy reads is the one `read_report` reported, not one
re-derived here from an error string (ADR-0057). A caller that would rather have an error for it has the
class in hand to build one from; the reverse is not true.

**The scope check runs last of the measurements and before the claim is
matched**, so a phase that both broke its scope and stayed silent is refused for
the write. The scope is the rule the phase exists to hold — it is the only thing
§9 gives a phase besides its gate — and an agent's silence beside a broken rule
is the smaller finding. The same placement means the check runs whatever the
claim said, including a claim of `DONE`: an agent that reports success while
writing outside its scope is refused, not believed.

**The session is invoked unwatched, with `None` for the bus.** The recorder's bus
is private and `subscribe` hands out a reader, so nothing can be lent without
widening `Recorder`; and the copy the dummy publishes carries no journal sequence,
so lending the recorder's bus would put a second, unnumbered version of every line
in front of the same subscriber that will be handed the numbered row seconds
later. The lines are not lost: `run_phase` writes one `AgentOutput` row per line
after the session ends, and each of those is published by the recorder as it is
committed. What is lost is *live* output for the duration of one phase, and that
cost is written down here rather than paid silently.

**The environment accessor is threaded, and one function was widened for it.**
`run_phase` is a two-line wrapper over `run_phase_with`, which takes
`&dyn Fn(&str) -> Option<String>` and passes it to `context::build_prompt_with` —
the same shape `paths::state_root_with` and `context::ensure_defaults_with`
already use. That required `build_prompt_with` to become `pub(crate)` rather than
private to `context`: the only file outside the task's list that this change
touches, and one line of it.

**The step moves no state and re-files no evidence.** No `apply` call —
ADR-0086 decided that a session's end is not the transition out of `running`, and
the phase's gate is the way out — and no second `write_evidence`: one attempt
files one record (ADR-0065), and `begin_attempt` already filed the attempt's
`context.md`.

## Alternatives considered

- **`Err` for a missing report, with a class derived from the error.** One shape
  for both refusals, and the loss is ADR-0057: the class would be read out of a
  message this function wrote, and the retry policy would then be driven by
  wording. It also loses the path, which is the actionable half.
- **`PhaseOutcome` with a `Vec<PathBuf>` in both arms.** Rejected as gold-plating:
  a phase that left no report is a failure about a file that was not written, and
  the paths its session touched are readable from the checkout and the journal
  without being smuggled out in a struct nobody consumes yet.
- **Read the report before the scope check and report the claim, noting the
  violation.** One fewer refusal, and the loss is the rule: the phase would
  return a `DONE` whose work the run has already refused to accept, and whoever
  read it would have to know to ask.
- **Trust `ReportClaim::Claimed{result: Done}` and skip the scope check for a
  reported success.** The literal reading of "the agent said it was done", and
  the exact thing §3's invariant 4 exists to forbid.
- **Lend `Recorder`'s bus to `invoke`.** Needs a `Recorder::bus()` accessor in
  `events.rs` — a file this task does not own — and would show a subscriber two
  events per line, one of them carrying the zero sequence the journal never
  issues. A later task that wants live output should decide what the provider's
  stream *is* (ADR-0081 is where the bus's semantics are argued), not quietly
  alias it to the journal.
- **Read the prompt from the real `XDG_CONFIG_HOME` in tests, or set the variable
  inside a test.** Both are forbidden by `docs/DESIGN.md` Conventions, and the
  second is a data race between tests in one process. Threading the accessor is
  the pattern the crate already uses twice.
- **`apply(PhaseEntered)` here, moving the task into the phase.** `PhaseEntered`
  is a journal row, not a transition; ADR-0086 settled the same question for the
  session's end, and the phase's gate owns the way out.

## Consequences

- A phase now has a readable boundary: `PhaseEntered` with no `AttemptFinished`
  is a session that died, `AttemptFinished` with no report is a session that
  ended without accounting for itself, and both are distinguishable from a phase
  that never started — which is what recovery needs and what §3's invariant 3
  asks for.
- `run_phase` returns `Ok` for a phase that achieved nothing. Whoever calls it
  must read the arm, and the two named tests that match the enum rather than
  `.is_ok()` are what keep that honest: a default of `Done` breaks them instead
  of passing quietly.
- The `Unreported` arm is not yet acted on. Nothing maps it to a
  `FailureRecorded` row or to `should_continue`, and that wiring is the next
  task's; until it exists, a phase that says nothing is a value the run holds and
  does not use.
- Live agent output for the duration of one phase is absent, and the gap is
  visible in the TUI's output pane, which will fill at session end rather than
  during it. Fixing it needs a decision about what the provider's stream means
  beside the journal's rows, plus the accessor `Recorder` does not have.
- `Runner::new` opens its journal with `Journal::open_for` and never calls
  [`Journal::with_secret_patterns`], so a session's printed lines are redacted by
  the built-in table only — a configured `secret_patterns` entry does not reach
  the `AgentOutput` rows this step writes. Reported as a finding rather than
  fixed here: the fix belongs to whoever owns the run's construction, and it is a
  one-call change with a test of its own.
- `context::build_prompt_with` is now crate-visible. Its doc says why, in the
  same terms `paths::state_root_with` uses, so the widening is a documented
  seam rather than an accident a later reader widens further.

# 0050. A provider is reached through one trait, and its type answers nothing

- **Status:** accepted
- **Date:** 2026-09-19

## Context

T054 fixes the shape of the provider layer: `Capabilities`, `Invocation`,
`Outcome`, and the `Provider` trait that runs a session. VISION.md §12 asks for
"a stable capability interface, with adapters" — `dummy`, Claude and Codex at
launch, Kiro/OpenCode/Goose "additive" behind them — and the task's done-when is
two clauses: "the trait is object safe" and "nothing in the core matches on a
concrete provider type". The first is checkable by the compiler. The second is a
promise about every task that comes after this one, which is why it is decided
here rather than discovered by the first adapter that gets in the way.

The forces that make it a decision:

- **Additive is the whole point.** A choice made by adapter identity has to be
  re-made at every `match` arm for every CLI added. A choice made by capability
  is made once, and a new adapter answers the same three questions.
- **One adapter, many readers.** VISION.md §5 has the TUI and the CLI consuming
  the same event stream, and one provider is configured per role for the length
  of a run (§12's "one provider implements, another reviews"). An adapter
  therefore cannot be held `&mut` by whoever is invoking it.
- **A provider talks while it runs.** `run_gate` already takes
  `Option<&Bus>`, so the parameter shape is the one the codebase uses. But where
  a gate's chunks have nowhere to go — `gate.rs` publishes nothing, because the
  catalog has no entry that can carry one — for a provider the entry exists:
  `AgentOutput { attempt, stream, text }` is one of the 19 kinds already defined.
  The identical parameter means something different in the two places, and that
  difference is what an adapter's contract has to state.
- **Two of these types outlive their writers, one type does not.**
  `docs/DESIGN.md` journals detection as `ProviderDetected { provider,
  capabilities, version }`, so `Capabilities` is a future record, deferred only
  by ADR-0011's rule that an entry arrives beside the `apply` arm that answers
  it. Nothing names an `Invocation` or an `Outcome` whole: `AttemptFinished`
  lists `exit_code`, `usage`, `session_id` and `model_reported` as its own
  fields. Encoding the call would fix bytes no reader reads, and would carry the
  prompt into a journal that never asked for it.
- **ADR-0049's rule reaches a session that never started.** A CLI that is not on
  `PATH` has no exit status. Inventing `-1` is the same substitution as a zero
  token count, one level up.
- **VISION.md §12 lists four detection questions** — structured output, model
  selection, usage telemetry, approval modes — and the task fixes three fields.

Measured, on object safety and on the encoding (rustc 1.98, serde 1,
serde_json 1, against the definitions this ADR describes):

- `Box<dyn Provider>`, `&dyn Provider` and `Vec<&dyn Provider>` all compile and
  dispatch, so the trait is object safe; the tests hold all three, which is what
  turns "a generic method was added to the trait" into a build failure in this
  module rather than a surprise in the runner.
- A capability record that omits a question is refused by name: `missing field
  `usage_telemetry` at line 1 column 49`. A `false` is therefore written as
  `false` rather than left out, which is the only way a record can say *cannot*
  instead of failing to say anything.
- An invented question is refused by name too: `unknown field `approval_modes`,
  expected one of `structured_output`, `model_selection`, `usage_telemetry``.
  The fourth item of §12 cannot ride along unmentioned.

## Decision

`crates/ktask-core/src/provider/mod.rs` holds one trait and three types, and
four rules:

1. **One door, `&self` only.** `Provider` is `name`, `capabilities`, and
   `invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome>`. It
   stays object safe; callers hold `dyn Provider`; no code in the core names an
   adapter type, and the two adapter types that do exist live in `#[cfg(test)]`
   so a production `match` on one cannot be written.
2. **A capability record answers all three questions.** `Capabilities` is three
   required booleans with `#[serde(deny_unknown_fields)]` and no
   `#[serde(default)]`, mirroring `Usage`. "Not detected" is recorded by the
   absence of the `ProviderDetected` entry, not by a field inside the answer.
3. **The call is not a record.** `Invocation` and `Outcome` derive `Debug`,
   `Clone` and `PartialEq` and no serde. `Outcome` takes `PartialEq` rather than
   `Eq` for the same reason `Usage` does — `cost_usd` is a float.
4. **Watched behaves as unwatched, and a refusal is an error.** `Some(&bus)`
   means an implementation hands what the session prints to the bus as
   `EventKind::AgentOutput` while the session runs; `None` changes nothing else
   about the call. Redaction stays in the journal's write path, where it already
   is. A provider that could not start returns `Error::Provider` naming itself,
   never an `Outcome` with a status it does not have.

## Alternatives considered

- **An adapter enum, and a `match` on it.** Rejected by the done-when, and by
  the arithmetic: each added CLI becomes an edit at every arm instead of an
  added file.
- **`&mut self` on `invoke`, so an adapter keeps its own state.** Rejected: it
  makes a provider exclusive to one caller for as long as it is invoked, which
  is incompatible with a TUI and a CLI reading one run, and every kind of state
  an adapter actually needs (a session id, a model id) comes back out in the
  `Outcome` instead.
- **`async fn invoke`, or a boxed-stream return.** Rejected: `async fn` in a
  trait is not object safe, and the workaround is `Box<dyn Future>` plumbing
  that buys nothing here — T057 already reads the two pipes on threads it owns,
  and publishing on this bus is synchronous.
- **`invoke(self: Box<Self>, inv: Invocation)` — one call per adapter.**
  Rejected: a retry re-runs the same invocation under a new attempt record, and
  a caller that handed its prompt over cannot do that.
- **`Option<bool>` per capability, to express "not detected".** Rejected: it
  moves the gap into the record, where every call site then chooses what unknown
  means. The absence of the detection event is the honest place for that fact,
  and it is one fact rather than three.
- **A fourth `approval_modes` field, to complete §12's list.** Rejected for this
  task: the field set is fixed by the task, and an approval mode is a policy
  answer no type has been named for. Reported as a finding rather than invented.
- **`dyn Provider + Send` now.** Rejected: nothing moves a provider off the
  calling thread yet. Adding a `Send` bound later is a widening a future
  non-`Send` adapter should fail loudly against, which is better than promising
  it now and revisiting it under pressure.
- **`exit_code: Option<i32>` on `Outcome`.** Rejected: the field type is given,
  and the state it would encode — killed by a signal rather than exited — is a
  fact about the process, which is T057's to report through the error channel.

## Consequences

- The runner, the scenario suite and the Claude/Codex adapters all reach a
  provider the same way, and a fourth CLI adds a file without editing a
  decision. Nothing in the core can tell them apart, which is the outcome T054
  asked for.
- `AgentOutput` has a named producer at last. The dummy adapter (T056) and the
  process supervision (T057) publish that shape, not a private one.
- `Capabilities`' encoding is fixed before its catalog entry exists, so
  `ProviderDetected` inherits a promise: the deferred-payload sample in
  `event.rs` writes `"capabilities":{}`, which this record refuses by name
  (measured above). The task that defines the entry updates that sample; it
  cannot relax this record to accept it.
- Nothing enforces a capability at `invoke`. Asking a `model_selection: false`
  adapter for a model typechecks today; the refusal belongs to preflight and the
  provider factory (T063), and is flagged there rather than decided here, since
  whether it is a refusal or a re-detection is a product question.
- "Watched behaves as unwatched" is a contract and a test, not a type. The dummy
  adapter's done-when — byte-identical output and journal across runs — is where
  it stops being a promise.
- `Outcome::exit_code` remains evidence that a session ran and never evidence
  that a task is done (VISION.md §3 invariant 4). The classifier (T064) reads
  what the session said alongside it, which is why this type carries `stderr`
  rather than a summary.

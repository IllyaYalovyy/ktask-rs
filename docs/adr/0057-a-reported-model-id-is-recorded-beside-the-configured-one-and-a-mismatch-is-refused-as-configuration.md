# 0057. A reported model id is recorded beside the configured one, and a mismatch is refused as configuration

- **Status:** accepted
- **Date:** 2026-09-19

## Context

VISION.md §12 spends one sentence on models: "Configured vs provider-reported
model IDs are both recorded; unexpected mismatches are rejected." T062 is the
task that makes that sentence executable, and it found three things that make
it a decision rather than one comparison.

- **One half had nowhere to live.** The configured id already had a home:
  `Invocation::model`, and `docs/DESIGN.md` names `model_reported` twice — once
  as a field of its `AttemptFinished` payload, once inside the `AttemptRecord`
  the flight recorder stores. `Outcome` carried `exit_code`, `stdout`, `stderr`,
  `usage` and `session_id`, and no field for a reported model. VISION.md §6's
  flight recorder promises the model id per attempt, and an adapter with nowhere
  to put a report cannot keep that promise whatever reads it later.
- **`crate::Error` has no provider-configuration variant.** `docs/DESIGN.md`
  fixes that enum and T001 fixed its payloads (ADR-0001), so a refusal has to be
  one of the variants that exist. `claude::configuration` and
  `codex::configuration` answer a bad model with `Error::Provider`, because each
  knows which adapter it is. A check on two string slices knows nothing of the
  kind.
- **No adapter can report a model today.** Claude and Codex are started with
  plain-text output — no structured format is requested, so
  `Outcome::usage` and `Outcome::session_id` are already `None` for every real
  session (ADRs 0049, 0054, 0055) — and `Dummy` answers
  `model_selection: false` because a scripted session runs on no model at all.
  So every session ktask can run right now belongs to the *missing report*
  half of §12, not the mismatch half.

## Decision

**Give the reported id a field, named as `docs/DESIGN.md` names it.**
`Outcome::model_reported: Option<String>`. The name is the documented one rather
than a rewording of it, for the reason ADR-0001 gave for `Error::Gate::kind`: a
field named as the document names it lets the task that journals it copy the
field rather than translate one. `Outcome` has no `Default`, so naming the field
is forced at every construction site — an adapter cannot omit a report by
forgetting about it.

**Make the check a function of two ids, not a step inside `Provider::invoke`.**
`check_model(configured: Option<&str>, reported: Option<&str>) -> Result<()>`.
The halves arrive from opposite directions — configuration through the
`Invocation`, the report back on the `Outcome` — and only the caller that accepts
an attempt holds both. An adapter that refused a session over the report it had
just given back would present a configuration fault as a provider failure, which
is the one classification VISION.md §7 cannot retry out of.

**Refuse a mismatch with `Error::Config` keyed `model`, naming both ids.** A
keyed `Config` error is how this core already says *a configured setting cannot
be honoured* (`dummy.rs` uses it for every refused scenario). It attributes
nothing that is not known: no adapter name is invented, and the key is the
setting an operator has to change. It is also what maps to
`FailureClass::ProviderConfiguration`, whose own definition names "an invalid
model" and which VISION.md §7 pauses for a human rather than spending an attempt
on. The detail says which id was asked for and which was reported, because
swapping the two sends a human to the wrong setting.

**Compare exactly, and allow the missing report.** A prefix, a case difference,
or a trailing space is a different id. The lenient direction has only one
failure: an attempt recorded as having run on a model nobody chose, and a
recorded attempt is the evidence a later task is reasoned from. A session that
reported nothing is accepted — model reporting is a detected capability
(`Capabilities::model_selection`), so refusing it would make every CLI without
structured output unusable — and it is *marked*: `model_reported` stays `None`
rather than taking the configured id, which is ADR-0049's rule that a guess may
not stand in for a report, applied to a model id. The pair
(`configured: Some`, `model_reported: None`) is the mark, and it is exactly the
pair `AttemptRecord` carries, so nothing is lost on the way to the journal.

## Alternatives considered

- **A `ModelReport`/`ModelAgreement` enum as the mark.** Rejected: `docs/DESIGN.md`
  records the fact as two `Option<String>` fields, which already separate all
  four states this task distinguishes. A third vocabulary in the provider layer
  would have to be flattened back into those two fields when T068 journals the
  record, and a flattening step is where a distinction gets lost.
- **`Error::Provider`, as the two adapters' own `configuration()` helpers do.**
  Rejected for this function: its signature has no adapter in scope, so the
  provider name would be invented, and `Error::Provider` reads as "the CLI could
  not run this session" — which is false here, the session ran and printed its
  work.
- **Refusing a session that reports no model.** Rejected: §12 makes reporting a
  detected capability, and the done-when of the task says a missing report is
  allowed. Refusing it would fail the dummy adapter and both launch adapters.
- **Case- or prefix-insensitive comparison.** Rejected with the lenient
  direction above.
- **Parsing a model id out of Claude or Codex prose to fill the field in.**
  Rejected: ADR-0049 refuses a guess standing in for a report, and neither
  adapter requests a structured format it could read the id from. This is why
  every field on the outcome of a real session is currently `None`, and why that
  is the honest state rather than a gap to paper over.

## Consequences

- An attempt is only accepted after `check_model` says so, and the check needs a
  caller: the provider factory (T063) is where a configured id first meets an
  adapter, and the runner that accepts an attempt is where the two halves meet.
  Neither exists yet, so **no production path calls this function yet**, and
  until an adapter reads a model id out of structured output the mismatch branch
  has no reachable input. The rule and the recording land now so the task that
  reaches a real report has a decision already made to plug into.
- The gap ADR-0054 recorded stays open: `classify(outcome, gates, git_error)`
  (T064) takes no error from a provider call, so the mapping from this
  `Error::Config` to `FailureClass::ProviderConfiguration` is still written
  nowhere. The key is `model` so that mapping has something to match on instead
  of a phrase out of a message.
- `Outcome` now carries four fields that are `None` because nothing asked the
  session the question. That is the shape of an untelemetered session, and a
  reader of an attempt record should expect it; the alternative was a record full
  of figures nobody measured.

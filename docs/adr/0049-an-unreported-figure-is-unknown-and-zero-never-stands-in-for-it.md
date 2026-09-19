# 0049. An unreported figure is unknown, and zero never stands in for it

- **Status:** accepted
- **Date:** 2026-09-19

## Context

T053 hands over the shape: `Usage` with four optional figures and a
`UsageSource` of `Provider`, `ParsedFromOutput` or `Unavailable`. The shape is
transcription; the done-when is the decision — "an unavailable figure is None
with source Unavailable; no code path substitutes zero for unknown". Whether a
`None` survives, or quietly becomes `0` at the first convenience, is what every
later reader of an attempt record inherits.

**The absence is routine, not exceptional.** VISION.md §12 makes usage
telemetry a *detected* capability: startup detection records whether a provider
reports it, so an adapter finishes sessions holding nothing, and the `dummy`
adapter that drives the scenario suite reports nothing at all unless told to.
VISION.md §6 then requires every preserved attempt to carry "tokens, and cost".
Those two sentences meet in this type, and one of them has to describe an
attempt that spent an unknown amount.

**The readers are not the writers.** VISION.md §17 ships the recording in v0.1
and puts "enforced cost/token/time budgets" in the backlog, so the code that
will act on these figures — remediation bounds, the TUI's live-run panel, a
later budget gate — arrives after the bytes are on disk and cannot tell a
measured zero from a substituted one. A zero says *this run cost nothing*, a
claim about the session; `None` says *nobody said*, a claim about the record.
Only one of those is checkable against anything.

**These are durable bytes.** `docs/DESIGN.md` names the field under its
`AttemptFinished` payload as `usage: Option<Usage>`, and ADR-0011 lists `Usage`
among the payload types that did not exist when the event catalog was cut. This
task supplies the type without supplying the entry — ADR-0011's rule that an
entry arrives beside the `state::apply` arm that answers it still holds, and the
catalog's absence test is unchanged. What does become fixed now is the encoding,
so the encoding is what gets thought about here.

Measured, on the encoding, with serde 1 and serde_json 1 (`/tmp/probe53`,
against the definition this ADR describes):

- **A missing key reads back as `None`.** serde treats an `Option` field as
  optional, so `{"output_tokens":2,...}` without an `input_tokens` key
  deserialises to `input_tokens: None`. The direction of that sloppiness is the
  safe one — an omitted figure becomes unknown, never zero — but it also means
  a writer that drops a key cannot be told apart from a session that reported
  nothing.
- **An invented key is refused**, and by name: `unknown field \`reset_at\`,
  expected one of \`input_tokens\`, ...`. That is `#[serde(deny_unknown_fields)]`
  on the struct, the same guard `GateResult` carries, and it is the only
  direction in which a record can be caught lying.
- **A `NaN` cost is written as `null`.** `serde_json` refuses to write a float
  JSON cannot hold, so a parse that produced `NaN` degrades to *unknown* rather
  than to a price. TOML cannot hold a `NaN` at all and fails the write, which is
  also fine: the journal is JSON.
- **TOML omits the absent figures** — an unavailable `Usage` writes as
  `source = "Unavailable"` and alone — and reads back as the same value, which
  is the first bullet again, seen from the other format.

**One aggregation was unavoidable, and it is where the rule reappears.** A total
is the first thing any caller wants. `input + output.unwrap_or(0)` is one keystroke,
and it reports a number for a session whose own record says the number is not
known: dropping an unknown half out of a sum is the substitution with the `None`
moved one line earlier.

## Decision

`crates/ktask-core/src/provider/mod.rs` holds what its providers reported and
nothing else. Each figure is an `Option`; `Usage::unavailable()` builds the one
value in which all four are `None` beside `UsageSource::Unavailable`, and
`Default` delegates to it rather than deriving.

`Usage::total_tokens()` is the single aggregation the type offers, and it
answers `Option<u64>`: `None` when either half was not reported, `None` when the
sum does not fit a `u64` (`checked_add`), and never a sum that includes cached
tokens — they are a subset of the input count, so adding them would bill the
same tokens twice.

`total_tokens` stays `Option`-valued on purpose. The later bounds check has the
signature `should_continue(bounds, attempts, elapsed, tokens: u64)`, so *some*
call site must convert. Keeping the unknown here means that conversion happens
in the open, at the call site that owns the policy, rather than inside the type
that holds the evidence.

The struct is `#[serde(deny_unknown_fields)]` and derives `Serialize` /
`Deserialize`, because the document that names it is a journal format. The
module is `pub mod provider`, with the two types re-exported at the crate root
beside the other durable-data types, so T054's trait and the adapters that
follow it have one home to be added to.

## Alternatives considered

- **`Default` as all-zero with `source: Provider`, which is what a derive
  gives.** Rejected outright: a derive cannot invent a `UsageSource` for a
  session nobody reported, and the variant it would pick is the first — `Provider`,
  the most trusted of the three — attached to four zeroes nobody measured. It is
  the substitution, written by the compiler.
- **A sentinel (`u64::MAX`, or `u64::MAX / 2`) for unknown.** Rejected: a
  sentinel is a number, so it joins sums and compares as a quantity, which is
  precisely the behaviour the rule forbids. It would also collide with a figure
  a provider could honestly report.
- **A sum that returns what it has**, treating an absent half as zero. Rejected:
  the done-when's rule one step removed. A total of 250 for a session that
  reported 250 output tokens and no input count is a figure no witness stated,
  and it is indistinguishable from a session that reported both.
- **Counting cached tokens into the total.** Rejected as a double count; the
  field is kept visible so a cost model can still price it differently.
- **Two nested types — `UsageReported { figures } | UsageUnavailable` — so the
  illegal combinations do not typecheck.** Rejected: the task fixes one flat
  struct with a `source` field, and the split would not remove the need anyway.
  A provider that reports output tokens but no cost is a *partial* report, and
  the closed variant either invents a fourth shape for it or loses the half it
  does have. `Option` per figure is what makes a partial report expressible; the
  source is what makes it provable which witness was partial.
- **Cost as an integer number of cents.** Rejected: the field type is given as
  `f64`, and both provider telemetry and a price table answer in a decimal
  float. The consequence is stated below rather than hidden.
- **`NEEDS_INPUT` on the cached-token semantics.** Rejected: it resolves from
  what a cached token is — input the provider did not recompute — and a decision
  that follows from a definition is not a product decision.

## Consequences

- A record can now say *unknown* in the only way this project records facts, and
  a screen can print it as unknown without lying. Nothing in this module closes
  the gap between `None` and `0`, so any future `unwrap_or(0)` is a visible
  choice at a call site rather than the type's default.
- A key dropped by a writer reads as unknown, not as zero — the conservative
  direction, and measured above. It is not the same thing as *detected*: if a
  later task must tell "the field was absent" from "the field was null", the
  field type has to become `Option<Option<T>>`, which is a journal format change
  and owes an ADR of its own.
- Exact cent arithmetic is not this type's promise. `cost_usd` is a float, so a
  budget comparison inherits float error; a task that enforces a dollar limit
  decides the tolerance, and should read this paragraph before inventing one.
- The bounds-check task inherits a `tokens: u64` parameter and no honest way to
  fill it from an unknown total. That is flagged to that task rather than
  decided here: "unknown" must not read as "under budget", and the fix belongs
  in `Bounds`, which is outside this task's file list.
- `UsageSource` has no `Display`. "unknown" is a rendering decision that belongs
  to the screen that needs it, and the journal is not where one is made.

# 0058. The provider factory is the only place a configuration word names an adapter

- **Status:** accepted
- **Date:** 2026-09-19

## Context

T063 wires `Config::provider` to an adapter:
`build(config: &Config) -> Result<Box<dyn Provider>>`. ADRs 0050 to 0057 built
the trait, three adapters, and the model rule, and between them they handed four
decisions forward to "the factory" rather than making them:

- ADR-0050: nothing enforces a capability at `invoke` — "the refusal belongs to
  preflight and the provider factory (T063) … since whether it is a refusal or a
  re-detection is a product question".
- ADR-0051: "Nothing here chooses a scenario. `Config::dummy_scenario_path` has
  no default, and `Dummy::load` refuses a path holding nothing; wiring the
  setting to a provider instance is the factory's, not this adapter's."
- ADR-0054: "`Claude::new(command, idle, hard)` is the seam T063's provider
  factory must fill: `Config` has no per-provider command key today, so the
  factory supplies the command word plus `idle_timeout_secs` and
  `attempt_timeout_secs`."
- ADR-0056: equality of a provider name "with the word an operator wrote is the
  factory's comparison".

What forces the shape of the answer is that three of the four are about things
`Config` does not have. Its field list is fixed by `docs/DESIGN.md` (ADR-0004
owns the defaults): there is no per-provider command key, and
`dummy_scenario_path` is documented as `None`. `crate::Error` is fixed too
(ADR-0001), so a refusal has to be a variant that already exists. And VISION.md
§12 keeps the adapter set additive, which means whatever this function does, a
fourth CLI has to be able to do it too by adding as little as this one did.

## Decision

**One function, in `provider/mod.rs`, and it is the only place a concrete adapter
is named.** `pub fn build(config: &Config) -> Result<Box<dyn Provider>>`, three
arms. Adding one of §12's backlog CLIs is one arm and one name. Everything above
this module holds `dyn Provider` and cannot tell the adapters apart (ADR-0050);
the naming of one is the single decision of its kind, so it is made once, from
data an operator wrote, rather than wherever a CLI happens to be reached. In the
test code `conformance.rs` still names `Dummy`, which is ADR-0056's fixture and
compiled for tests only.

**`dummy` is its scenario file, and no file is a refusal.** The path comes from
`Config::dummy_scenario_path` and is the whole of what that arm hands over; a
`dummy` whose setting names no file answers `Error::Config` keyed
`dummy_scenario_path`. A built-in scenario was the live alternative — §12 does
call it "a built-in `dummy` provider" — and it is exactly what ADR-0051 refuses:
if the responses live in code, every end-to-end case is a code change and the
deterministic half of "deterministic provider" is an agent's promise rather than
an artifact a reviewer can read. The consequence is stated rather than smoothed
over: the documented default configuration does not build a provider, and the
refusal names the key to set.

**The command word is the adapter's own name.** `claude` starts `claude` and
`codex` starts `codex`, a bare word the adapter searches along `PATH` when a
session starts. `Config` has no per-provider command key and this task did not
invent one. Nothing is looked up *for* in the factory: a build that probed first
could not tell an operator what their configuration named on a machine that
lacks the CLI, and a missing binary is already a refusal with a class of its own
at the place that starts it (ADR-0054), which is also what `ktask-rs doctor`
reports. `idle_timeout_secs` and `attempt_timeout_secs` arrive as constructor
arguments rather than as lookups, so a session stays reproducible from the
arguments its adapter was built with (ADRs 0053, 0054).

**The match is exact.** `Config::provider.as_str()` against the three words — no
trim, no case fold, no prefix. This is ADR-0056's deferred comparison, made
here, and the reason is the one §12 gives for a mismatched model id: a near miss
resolved into a real adapter runs the attempt on a CLI nobody chose and files its
evidence under the wrong provider, which no retry can undo because the evidence
is already wrong.

**An unknown word is `Error::Config` keyed `provider`, naming the word and
listing the valid names.** The list the refusal prints is the same array the arms
match on, so it cannot go stale against the set that actually builds. `Config`
rather than `Provider` for the reason ADR-0057 reached the same way: a word that
names no adapter has no adapter name to put in `Error::Provider`'s payload, and
this is a configured setting that cannot be honoured.

**No capability is enforced here.** `Config::model` is not weighed against the
chosen adapter's `Capabilities::model_selection`. ADR-0050 left that a product
question — refusal or re-detection — and a factory is the cheapest place to
answer it silently, which is the wrong reason to answer it. `check_model`
(ADR-0057) stays the one model rule this core holds, and it still has no
production caller.

## Alternatives considered

- **A built-in five-step `dummy` scenario**, one per §12 response word. Lost on
  ADR-0051: it is a script in code, invisible to a reviewer and editable only by
  a commit. §12's sentence is about the adapter being first-class and its
  responses being declared; the file is where they are declared.
- **`Error::Provider` for an unknown word.** The right-looking class, the wrong
  payload: its `provider` field is the adapter a failure is attributed to, and
  nothing was attributed because nothing was chosen. ADR-0054 refused the mirror
  swap for the same reason — a variant chosen to look like a class rather than
  the class the message states is a decision made twice.
- **A registry: a `&[(name, constructor)]` table walked instead of matched.**
  Data-driven, and it buys nothing at three adapters while costing each arm its
  own construction: a `Dummy` is loaded from a path and a `Claude` is built from
  a word and two clocks, so every row needs its own closure and the arms do not
  disappear — they just move somewhere harder to read.
- **A `ProviderKind` enum that `Config::provider` deserialises into.** It moves
  the provider set into `Config`, whose field doc says the name is a `String`
  precisely because the set is data-driven and a run reports its provider as
  text; and it would refuse a bad word while *loading* configuration, far from
  the layer that owns the adapters and with no access to what they accept.
- **Probing `PATH` in `build` so an unusable provider fails at construction.**
  Rejected with ADR-0054: a constructor that answers a filesystem question makes
  a machine without the CLI unable to say what it was configured with, which is
  exactly what `doctor` exists to print.

## Consequences

- `Config::default()` is not a runnable provider configuration. Any caller that
  reaches for a provider has to handle the refusal naming `dummy_scenario_path`
  rather than assume the default runs; `doctor`'s preflight reports it as the
  configuration failure it is.
- A `provider_command` setting is now the obvious next request from anyone whose
  CLI is not on `PATH` under its own name. It is a `Config` key — `docs/DESIGN.md`
  owns the field list and ADR-0004 the defaults — so it belongs to a task that is
  allowed to add one. The factory's command word is the single seam that changes
  when it arrives.
- Provider selection by task type (VISION.md §12's implement/review pair) has
  gained nothing and needs nothing: the trait takes `&self` everywhere, so one
  run may hold two built adapters, and the caller holding the queue is the one
  that decides which configuration applies to which task.
- The model-versus-capability question is still open, and it now has one honest
  home: the place that hands `Config::model` to an adapter, which is nowhere yet.
  Recorded here so the next task inherits the question rather than an accident.
- The two clocks are unobservable through `Provider`. Nothing above the layer can
  see whether `idle_timeout_secs` reached an adapter's idle slot or its hard one,
  and the only test that could check would have to name the concrete adapter —
  the thing this task's done-when forbids. The field documents on both adapters
  say which setting feeds which slot; a swapped pair would make every session die
  on silence rather than on length, and it is a gap in the suite that is named
  rather than papered over.

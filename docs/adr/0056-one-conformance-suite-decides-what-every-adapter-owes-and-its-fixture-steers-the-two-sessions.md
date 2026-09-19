# One conformance suite decides what every adapter owes, and its fixture steers the two sessions

- Status: accepted
- Date: 2026-09-19
- Task: T061

## Context

VISION.md §12 makes the provider layer additive: `dummy`, Claude and Codex at
launch, and further CLIs behind them, all reached as `dyn Provider` so nothing
above the layer can tell them apart. ADR-0050 wrote the trait and the rule that
goes with it — an adapter's *identity* answers nothing, because a `match` on a
concrete adapter is a decision re-made for every CLI added. Three adapters exist
now (`dummy`, `claude`, `codex`), and each is tested by the tests its own author
thought of. That is the gap T061 closes: with per-adapter suites, the difference
between two adapters' tests is a behavioral difference between the adapters that
no code can see, and the fourth CLI from the backlog inherits whichever rules its
author happened to write down.

The task named the four rules and the shape: `conformance_suite(p: &dyn
Provider)`, asserting a non-empty name, capabilities stable across calls, an exit
code of zero with non-empty stdout for a session that worked, and a non-zero exit
code surfaced rather than an error for a session that failed. Done-when: adding
an adapter requires no new test code.

Four things made the shape a decision rather than a function.

- **The trait cannot be told to fail.** `Provider::invoke` takes `&self`, and
  `Invocation` carries prompt, model and directory and nothing else. `Dummy`'s
  `invoke` documents that the prompt decides nothing — "a response read out of
  prompt text would make every scenario a function of wording no step declared".
  So the rule about a failing session cannot be driven through the call the suite
  is allowed to make.
- **A name is the only thing an adapter answers about itself**, which is why the
  rule is about emptiness and not about a value: the suite holds `dyn Provider`
  and has no expected name to compare against.
- **The crate forbids what an assertion is.** A suite that reports a violation
  panics; the workspace lints make `panic` an error in this crate because a
  supervisor that panics loses the run it was supervising, and `clippy.toml`
  allows those macros in tests specifically because "in tests they are the
  clearest way to assert".
- **A scratch directory is a dependency-shaped decision.** The suite runs two real
  sessions, so it needs a directory a session may work in. `tempfile` is an
  optional dependency behind this crate's `testing` feature (ADR-0042), a
  dev-dependency for tests, and must not become a cost a run pays.

## Decision

**One function, four rules, in `crates/ktask-core/src/provider/conformance.rs`.**
`conformance_suite(p: &dyn Provider)` asserts the name is not blank, that two
calls of `capabilities` agree *and* that the answer did not move across a
session, that the first session answers `Ok` with exit code zero and something on
stdout, and that the second answers `Ok` with a non-zero status. Any non-zero
status passes the fourth rule: the suite refuses a *missing* status, not a status
it dislikes, which is why the scripted fixture exits 7 and a `Dummy` step exits 1.

**The adapter's fixture decides which session fails, and the suite says so.** A
`Dummy` is built on a scenario whose first step succeeds and whose second fails;
a real CLI under VISION.md §15's optional smoke tier is built on a command that
exits non-zero on its second call. An adapter handed to the suite that was not
configured that way is refused, and that is the right verdict: an adapter whose
failure path the suite cannot reach is an adapter whose failure path has never
been tested. The alternative — driving a failure through the call — is rejected
below.

**Sessions run unwatched.** `bus` is `None`. The trait already says a provider
that behaves differently when watched cannot reproduce its scenario headlessly,
and that claim belongs to the bus and the recorder's tests, not to an adapter's
conformance.

**The refusals are the delivery, not an appendix.** Each rule is run against a
`Scripted` adapter that breaks that rule alone, with `#[should_panic(expected =
...)]` pinned to the message that names the rule. `Dummy` is the reference
adapter that must pass. Without the refusals, a rule that stopped being enforced
would be indistinguishable from a rule that holds — the exact false confidence
`scripts/review-tests.sh` exists to count.

**Compiled for tests only** (`#[cfg(test)] pub mod conformance;`), beside the
adapters it checks. It keeps `tempfile` a dev-only cost, keeps the crate's
no-panic rule intact in everything that ships, and costs nothing: every adapter
this suite can build lives in this crate.

## Alternatives considered

- **Driving the failing session with a magic prompt.** One reserved string that
  makes any adapter fail would give the fixed signature a working second rule.
  Rejected because it puts the instruction in the prompt, which is precisely what
  `Dummy::invoke` refuses to read, and because the prompt is preserved as evidence
  of the work an agent was asked to do — a run would carry a sentence that was a
  test fixture.
- **A second argument: `conformance_suite(p, expects_failure_on: 2)`.** Honest,
  and it keeps the prompt clean. It lost on the done-when: a parameter an adapter's
  author must fill is a decision re-made per adapter, which is what ADR-0050
  removed from this layer. It is the shape to revisit if a suite consumer ever
  needs to steer a session that the fixture cannot.
- **A `Conformance` trait each adapter implements.** Puts the answers in the
  adapter, so registering one means writing code about itself: the suite would
  testify to whatever the adapter claimed. `dyn Provider` is the point.
- **Returning `Vec<String>` of violations instead of asserting.** Errors as values
  is this crate's rule for a *run*, and it would have been the consistent choice
  for shipped code; it fails the done-when, because every adapter's test then
  needs the assertion that the vector is empty — new test code per adapter, in the
  one place new test code is forbidden.
- **Shipping the suite publicly, or behind the existing `testing` feature.**
  A public function whose contract is a panic is a public API that panics, in a
  crate that forbids panics on pain of losing a run, and the feature-gated form
  invites a non-dev build to pay for `tempfile`. Nothing outside this crate builds
  an adapter today. Revisit when one does: the module is written to be lifted, and
  its signature does not change.
- **Refusing a padded or non-ASCII name.** The rule the task names is emptiness.
  A name that is only whitespace is not a name by any reading — it survives no
  `Error::Provider` and no configuration file — so `trim` is where the rule stops.
  Equality with the word an operator wrote is the factory's comparison (T063), and
  each adapter's own tests already pin its name.

## Consequences

- A fifth adapter from §12's backlog pays one test line: build the fixture, call
  `conformance_suite`. It cannot pass by omitting assertions, because the
  assertions are not in its file.
- The suite is only as strong as its refusals: a rule with no scripted
  non-conformance beside it can rot unnoticed. Adding a rule to
  `conformance_suite` means adding a `Scripted` case that breaks it, and the
  module is arranged so the two changes are adjacent.
- `Claude` and `Codex` are not run by the default suite: they are constructed from
  a command on `PATH`, and conformance for them needs a stub command that answers
  two sessions, which is VISION.md §15's optional smoke tier and T062's preflight
  territory, not a default test that spends a network and money.
- A provider whose success is silent, or whose failure arrives as an error, fails
  here rather than in a run. The second is the one that would otherwise be read as
  `provider unavailable` and retried instead of classified.
- The suite says nothing about what a session publishes to a live view, so an
  adapter that withholds `AgentOutput` is not caught by conformance; that stays
  with the trait's own tests until a run wires the bus and the recorder together.

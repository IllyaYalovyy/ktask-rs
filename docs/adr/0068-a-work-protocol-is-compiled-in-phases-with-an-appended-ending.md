# 0068. A work protocol is compiled-in phases with an appended ending

- **Status:** accepted
- **Date:** 2026-09-20

## Context

VISION.md §9 says a run holds two state machines. `TaskState` is the outer
one — custody of a task, the same for every task — and what happens *inside*
`running` is the work protocol, "an opinionated definition of what it means to
work on a task": a sequence of typed phases, each declaring its write scope, its
gate command, its provider, and the evidence it records. Before this task the
vocabulary existed in two unconnected places — `Phase` carried all twelve
steps of all three protocols, and the journal stored a `phase` with every
`PhaseEntered` — but nothing said which phases a protocol actually visits, in
which order, or what each one is allowed to touch. The queue could display a
phase nobody had declared reachable, and `Phase::Red`'s promise that production
paths stay read-only was prose in a doc comment.

T073 fixes the shape: `PhaseSpec { phase, write_scope, gate, records_evidence }`,
`WriteScope { All, TestsOnly, None }`, `Protocol { name, phases }`, and the two
constructors v1 ships, `direct()` and `tdd()`. Its done-when is one rule: every
protocol ends with Verify then Publish, and a constructor that omits them fails
a test. The rule is §9's constitution read at its narrowest — "every protocol
must terminate in the mandatory verify-publish gates, every loop must be bounded,
gates cannot be removed" — and it is the rule that stops this module becoming a
workflow DSL by the back door. Six things are decisions rather than steps.

## Decision

1. **A protocol is a value built in code, not data loaded from a file.** The two
   constructors are the whole set, and `Protocol` is assembled from struct
   literals. §9 is explicit — protocols "are not user-definable in v1", "There
   will never be a free-form workflow DSL", and "How you work is configurable;
   what done means is not" — and the half of that sentence this module owns is
   the second clause. A protocol read from configuration is a protocol an
   attempt can rewrite, which is ADR-0067's hole reopened in a file this module
   would have to trust.
2. **The ending is appended, not written.** No constructor lists
   `Phase::Verify` and `Phase::Publish`. Each hands its body to one private
   `assemble`, which extends it with the completion pair and names the result,
   so "the two of them decided not to verify" has no spelling. `Protocol::phases`
   is a public field because the runner and the TUI read it, so the field cannot
   itself refuse a hand-built sequence that skips the ending: the door is closed
   by the test ledger (`every_protocol()`), which holds every protocol v1 can
   build and checks each one's last two phases, plus its whole declared
   sequence. A third constructor that is not in that ledger is a constructor no
   test has judged, which is why the ledger carries a doc comment saying so.
3. **A phase names one gate, and the two completion phases name it differently.**
   `Verify` declares `GateKind::Verify` — the suite §8 calls "not optional, not
   skippable by config in strict mode". `Publish` declares `None`, because
   `GateKind` has no publication variant (ADR-0034 pins the kind set to what the
   documents name) and §10 proves publication a different way: by the tip a
   fetch brought back. The *other* completion gates — lint, format, build,
   privacy, flake — are the runner's completion set, not phase declarations:
   `Option<GateKind>` says which check decides this phase, and a phase that held
   a list would be a second, rival definition of the same set.
4. **Every phase an agent works in declares the targeted check.** `Implement`,
   `Red`, `Green` and `Refactor` each declare `GateKind::Targeted`, which is
   §8's `targeted_test_command` — "the fast check of an edit loop … run while the
   agent is still working rather than after it stopped". §9 spells that check for
   red, green and refactor; `direct`'s single implementation phase gets it for
   the same reason in one fewer step: a phase that declares no check ends on the
   agent's account of what it did, and §3's invariant 4 is exactly a refusal of
   that.
5. **A write scope is a kind of access, not a set of paths.** Three variants,
   and `TestsOnly` means "whatever the project's `test_globs` name"
   (`docs/DESIGN.md`) rather than naming a glob itself, because test paths are
   language configuration — a Rust profile and a Flutter profile speak different
   ones, and a protocol that embedded them could not be the same object for
   both. Enforcement is a later task (T076's `check_scope`) and reads the git
   diff, never the agent's report of what it touched; `Verify` and `Publish`
   declare `None` because §10 classes a dirty tree at verification time a
   `policy_failure`.
6. **`records_evidence` is true exactly where a phase's claim is not a gate
   result.** Red and green: §9 names them ("RED and GREEN evidence (command,
   output, tree hash) is stored with the attempt"), and they are the only phases
   whose claim — *this test failed before the implementation existed* — cannot be
   recovered afterwards. That is the whole reason §9 says no final test run can
   prove tests were written first. Publish, because no gate reports what it
   proved and §3's invariant 7 demands the proof exist. Refactor and `direct`'s
   `Implement` are false: their claim is that a gate stayed green, and that gate
   already files a `GateResult` with the attempt, so a phase artifact would be a
   copy with a second chance to disagree.

## Alternatives considered

- **A fallible `Protocol::new(name, phases) -> Result<Protocol>` that validates
  the tail.** It makes the rule checkable at runtime by any caller, which is
  worth something — but T073 fixes the constructors as infallible
  (`-> Protocol`), so calling it would mean `.expect()` in library code, which
  this crate forbids (`Cargo.toml`: a supervisor that panics loses the run), and
  the alternative — protocols that fail to build — moves a programming mistake to
  a runtime error at the moment a task is starting. Validation is still the right
  answer for whatever later assembles protocols from data; it is not the shape of
  a module whose whole input set is two functions.
- **Typestate: a `Protocol` type parameterised by its last phase.** Refuses an
  unterminated protocol at compile time, which is stronger than a test. It lost
  on cost: two protocols, one ending, and a type the TUI and the queue would
  have to name in order to display a phase.
- **Checking the ending where protocols are used rather than where they are
  made.** The runner could refuse a protocol with the wrong tail. That is
  defence in depth, not the definition: by the time a runner holds a protocol it
  has already started an attempt against it.
- **`phases: Vec<Phase>` beside a parallel vector of scopes and gates.** One
  `PhaseSpec` per phase keeps a phase's four facts in one place, which is what
  makes an incomplete declaration impossible to write — a `Vec<Phase>` can be
  paired with a shorter vector of scopes.
- **Deriving `Serialize`/`Deserialize` on `Protocol`.** Rejected: `Phase` and
  `GateKind` derive them because the journal *stores* them, and nothing stores a
  protocol. The queue row holds its name (`docs/DESIGN.md`: `protocol TEXT`,
  NULL meaning the configured default) and `AttemptStarted` journals a `String`;
  T075 maps that word back through the two constructors. A `&'static str` field
  that deserialized would be a type that lies about where its bytes came from.
- **Naming the type `ProtocolPhase`,** as `docs/DESIGN.md`'s module list spells
  it. `PhaseSpec` is what T073 writes, and the task body is authoritative over
  that line; the drift is reported rather than fixed here, since DESIGN.md is
  not this task's file.

## Consequences

- The two protocols are now one readable list each, and a phase can be refused
  for editing a path its own declaration did not open — which is what T076 needs
  to enforce and what the inspector needs to display.
- Nothing consumes the model yet, exactly as nothing consumed `should_continue`
  when ADR-0066 landed. Selection per task is T075, scope enforcement T076,
  red/green verification T077–T078, and the call site that walks `phases` is the
  runner (T091).
- `PhaseSpec` carries no provider/model and no loop bound, though §9 lists both
  among what a phase declares. T073's field list is the authority for this task
  and §9 marks the phases that would need them (`Review` by a second provider,
  `spec-first`'s bounded iteration) as v0.2/backlog, so both are recorded as
  absent rather than invented as fields nothing sets. Adding them later is
  additive: `PhaseSpec` is built in two places and both are in this file.
- A protocol that wants a completion phase *before* verification — a task that
  publishes and then verifies — cannot be built without editing `assemble`, and
  the test over the ledger fails first. If §9's composition entry is ever built
  from phase primitives, `assemble` and that ledger are the two places the
  constitution has to move to, and the ledger becomes the registry it should
  already be.
- `records_evidence` is a declaration with no writer yet. When the runner stores
  red/green artifacts it stores what §9 names — command, output, tree hash — and
  a phase flagged `false` gets a gate result and nothing else; if that split ever
  stops covering a phase's claim, the field is where the fix is recorded, which
  is why it is a named bool rather than an inference from `gate.is_none()`.

# 0034. The gate-kind set is the kinds the documents name

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T038 defines `pub enum GateKind` "with the nine variants in docs/DESIGN.md".
Counting what the documents actually hold gives three different answers, and
the task prompt is not one of the sources it cites:

- `docs/DESIGN.md:133` spells the enum with **seven** variants — `Baseline,
  Targeted, Verify, Lint, Format, Build, Privacy`. Nothing else in that file
  names a gate kind; its configuration table does carry `flake_runs: 5`, a
  setting only a flake gate can use.
- VISION.md §8, the section T038 lists under *Refs*, defines the verification
  profile as **eight** runner-executed commands: `baseline_command`,
  `targeted_test_command`, `verify_command`, `lint_command`, `format_command`,
  `build_command`, `privacy_command`, `flake_command`. §14 puts
  `flake_command` in v0.2, which §14 states is "equally required for v1".
- The task plan's own T039 adds exactly those **eight** fields to `Config` and
  builds a `Profile` from them, so it needs one kind per field or it must drop
  the gate VISION.md §8 names.
- **Nine** appears nowhere. The plan uses "its nine variants" for
  `FailureClass`, which really does have nine (`docs/DESIGN.md:138`), and the
  phrase reads as carried over from that task. ADR-0011 counted the gate kinds
  as seven when it deferred `GateStarted`.

A ninth variant cannot be chosen without inventing a gate no document names —
publication is a git transaction rather than a gate, and a queue entry that
waits for a human is a task, acknowledged by `GateAcknowledged { by, at }`, not
a `GateKind`. The count is therefore a prompt error, and the set is a reading
question rather than a product decision.

## Decision

`GateKind` has eight variants: the seven `docs/DESIGN.md` spells out, plus
`Flake`, which VISION.md §8 lists among the profile's commands and which the
next task needs a kind for. `Targeted` keeps the name `docs/DESIGN.md` spells
for the gate VISION.md calls `targeted_test_command`, because DESIGN.md fixes
type names and config.rs fixes key names.

Two spellings, each for one audience. Serde uses the variant name
(`"Verify"`), which is how every stored enum here is written — `FailureClass`,
`Phase`, `Stream`, `PauseReason` all derive without a rename, and ADR-0011's
forward-looking journal sample already expects `"Verify"`. `Display` gives the
one-word lower-case form (`verify`), which is what `Error::Gate` holds as text
per ADR-0001 and what `rerun-gate --gate <kind>` will be typed as.

`Profile` refuses to load in two cases beyond a malformed document: no
`Verify` gate, and one kind configured twice. The first is VISION.md §8's
"not optional, not skippable by config in strict mode", enforced where a
profile comes into existence rather than in each caller. The second is what
makes `Profile::get(kind) -> Option<&Gate>` a promise rather than a coincidence.

## Alternatives considered

- **Implement the seven literally.** Rejected: VISION.md is authoritative and
  its §8 is the task's own *Refs*, `flake_runs` already exists in DESIGN.md's
  defaults, and T039 declares `flake_command` and owns `gate.rs` — so the
  eighth variant would land there one task later with no ADR beside it.
- **Invent a ninth kind to match the prompt.** Rejected: every gate kind names
  a check the runner executes, and no document names a ninth check. Naming one
  would put a gate in the type that no phase, event, or screen can account for.
- **`#[serde(rename_all = "snake_case")]`, so a document reads `verify`.**
  Rejected: durable data in this project is written by variant name, and one
  enum renaming itself makes the journal the only place with two conventions.
  A profile document is built from `Config` fields by T039, not typed by hand.
- **Report `NEEDS_INPUT`.** Rejected: the three candidate sets are all named in
  documents that exist, and choosing their union decides nothing a human has to
  own. Recorded here, and reported as a finding, instead.

## Consequences

T039's eight `*_command` fields map one-to-one onto the kinds, so
`profile_from` cannot silently drop a configured gate; the test that counts
`GateKind::ALL` fails if a kind arrives that no document names, or one leaves.

Two documents owe a correction, neither of them editable by this task because
neither is in its file list. `docs/DESIGN.md` should carry `Flake` in the enum
so the "exact definitions" list matches the profile it documents, and the plan's
T038 line should say eight. `docs/DESIGN.md:218`'s `GateStarted` entry still
names a payload field `kind`, which its `#[serde(tag = "kind")]` already owns
(ADR-0011) — that collision is unaffected by this task and stays with the task
that emits the event.

If a human decides the set is wrong, the change is one variant line and one
array entry; `Profile::validate` and the loader need no edit, because neither
names a specific kind but `Verify`, and `Verify` is not in question.

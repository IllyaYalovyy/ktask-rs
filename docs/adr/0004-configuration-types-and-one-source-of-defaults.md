# 0004. Configuration types and one source of defaults

- **Status:** accepted
- **Date:** 2026-09-16

## Context

`docs/DESIGN.md` (Configuration defaults) fixes 22 keys and the value each takes
when nobody sets it, and fixes `deny_unknown_fields` as the rule for a key that
is not one of them. It does not state the Rust type of any field — only that two
of them default to `None`.

That is a decision rather than a detail, because the type is what a
configuration file is allowed to say. With `deny_unknown_fields` and typed
fields, `attempt_timeout_secs = "4 hours"` is an error at load time, and a width
chooses which documents are rejected: a `u32` byte count reads 2 GiB happily and
then refuses 8 GiB, so an operator could not write a setting the design
documents. The widths also decide whether the code that later reads a config
casts: `context_budget_bytes` is compared against `String::len()`, a `usize`.

Second: where the deserializer gets the value for a key a document omits. Stated
per field, it is a second copy of every default, and the Configuration screen —
which shows the effective configuration and where each value came from — is
exactly the place a disagreement between the two copies becomes visible.

Third: `provider` and `default_protocol` could be enums. `docs/DESIGN.md` stores
both as text elsewhere (`AttemptStarted { protocol: String }`, the `tasks`
table's `protocol TEXT` column), and the protocol and provider sets are
`spec-first`/`tdd`/`direct` and `dummy`/`claude`/`codex`, both of which a later
task widens.

## Decision

One `pub struct Config`, one field per documented key, field name equal to TOML
key. Types:

- **`u64`** for the five quantities measured in seconds and for
  `min_free_disk_bytes`, which comes from a filesystem statistic and must be
  able to hold a disk.
- **`usize`** for the three sizes that bound something inside the process —
  `context_budget_bytes`, `failure_bundle_bytes`, `output_ring_lines` — because
  they are compared with, and slice, in-memory buffers whose lengths are
  `usize`. A `u64` there asks every use site for a narrowing conversion it
  cannot prove safe.
- **`u32`** for the counts: `max_attempts` and `max_remediation_attempts` share
  the width of `AttemptId`, and `circuit_breaker_threshold`, `flake_runs` and
  `retention_days` count things no run will have many of.
- **`String`** for `provider`, `mainline_remote`, `mainline_branch` and
  `default_protocol`; **`Option<String>`** / **`Option<PathBuf>`** for `model`
  and `dummy_scenario_path`, the two keys documented as `None`; **`Vec<String>`**
  for `test_globs` and `secret_patterns`, which are patterns matched elsewhere,
  not compiled here.

`#[serde(default, deny_unknown_fields)]` on the container: deserialization
starts from `Default::default()` and overlays the keys the document sets, so
`impl Default` is the only place a default value is written, and an empty
document is `Config::default()` by construction rather than by two lists that
happen to agree.

## Alternatives considered

- **`#[serde(default = "…")]` per field.** Twenty-two more functions, each a
  second copy of a value `Default` already states, free to drift from it. Lost
  to the container attribute, which cannot drift.
- **`#[serde(skip_serializing_if = "Option::is_none")]` on the two `Option`
  fields.** Unnecessary: `toml` omits a field whose value is `None` on its own,
  and omitting it reads back as `None` because of the container default. A
  round-trip test pins both halves rather than trusting that.
- **Enums for `provider` and `default_protocol`.** Would force this task to fix
  the variant sets for the provider and protocol tasks, and would still have to
  reject an unknown string at load time — the same error, one type further
  away. It also disagrees with how the design already stores both: as text.
- **`u32`, or `usize`, everywhere.** `u32` byte counts reject a legal 8 GiB
  free-disk floor. `usize` for the timeouts and the disk floor puts a
  platform-dependent width on values that come from a file and from a
  filesystem, and narrows on every comparison with a `u64` statistic.
- **`u64` for `output_ring_lines`.** The design pairs it with a `dropped: usize`
  count on the same ring; a `u64` capacity makes the two arithmetic operands
  differ in width in the code that drops the oldest line.

## Consequences

- A configuration file is checked as it is read: a wrong type, a negative count,
  a key that is not documented. Each is an error naming what it found, and the
  tests pin the type name in the message, so widening or narrowing a field
  fails a test instead of passing unnoticed.
- `impl Default` is load-bearing twice: it is the documented default *and* the
  base layer of deserialization. Layered loading (the `Source` half of
  `config.rs`) must therefore treat it as the lowest layer, not restate it.
- `output_ring_lines` cannot be configured on a platform with a 16-bit address
  space. Not a target: the tool shells out to `git` and embeds SQLite.
- If a `Protocol` or `Provider` type is introduced later, retyping those two
  fields is local to this struct as long as each still reads and writes the same
  string.

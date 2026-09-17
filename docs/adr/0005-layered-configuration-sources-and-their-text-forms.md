# 0005. Layered configuration sources and their text forms

- **Status:** accepted
- **Date:** 2026-09-16

## Context

`load` has to merge four layers — defaults, global document, project document,
environment — and say afterwards which layer won each key. Two things are not
obvious, and both are decisions rather than details.

First, **the environment carries text, and every setting has a type** (ADR-0004).
Nothing in `docs/DESIGN.md` says what `KTASK_TEST_GLOBS` is supposed to look
like, so the convention has to be chosen once and written down: a per-key naming
rule, and a text form per type. Chosen badly, an operator sets a variable that
is quietly ignored, which is the failure this whole module exists to make
impossible.

Second, **a layer has to be distinguishable from the absence of a layer.**
Deserializing a document straight into `Config` cannot do that: the container
`#[serde(default)]` fills in what the document omitted, so afterwards nobody can
tell `provider` written by the project from `provider` left at its default — and
provenance is exactly that distinction. The same problem recurs for the
environment, where the answer is one string per key.

Third, an empty value. `docs/DESIGN.md` Conventions forbids a test setting a
process environment variable, so `load` takes an accessor rather than reading
`std::env::var` itself; that leaves open what `KTASK_MODEL=` means. ADR-0002
already answered the analogous question for `XDG_CONFIG_HOME`: empty means
unset, because the consumer wants a value it can use.

## Decision

**One key table, one name per source.** `KEYS` lists each documented setting
once: the TOML key, the `KTASK_`-prefixed upper-case variable spelled out
literally, and the field to write. The variable name is not derived at runtime
from the key, so it is greppable and the table is the whole answer to "how do I
override this?" Two layers of documents and one of variables then apply in a
fixed order, each writing only the keys it holds, and each write records the
layer.

**A document is read as a map of untyped values**, not into `Config`:
`BTreeMap<String, toml::Value>`. Its keys are checked against the table — an
undocumented key is refused, as `deny_unknown_fields` refuses one in a single
document — and each value is then read as the type of the field its key names.
That is what makes "did this layer set this key?" answerable.

**Environment text is typed by the field it lands in**: integers parsed as the
field's own width, strings and paths taken as written, lists split on commas
with each entry trimmed and empty entries dropped. A value is trimmed, and
what is left empty carries no setting at all — `KTASK_MODEL=` sets no model. A
list of separators alone (`KTASK_TEST_GLOBS=,`) is the one way to set an
explicitly empty list through the environment.

**A file that is not there is not a layer.** `ErrorKind::NotFound` on either
document path skips that layer; any other read failure is returned as
`Error::Io`, because settings somebody wrote are about to be ignored.

**Failures name their subject.** A document that is not TOML is refused with
the file as the key and `not a valid TOML document` in the detail. A key-level
failure carries the key as the key and a detail that starts with the origin —
``file `/path` `` or ``environment variable `KTASK_MAX_ATTEMPTS` `` — so one
message says what and where. `Error::Config` already had those two slots; no
new variant was needed.

**Provenance is a `#[serde(skip)]` field on `Config`**, a map from key to
`Source`, and a key with nothing recorded reports `Source::Default`.

## Alternatives considered

- **A mirror struct of `Option<T>` per field, one per document layer.** Twenty-two
  fields duplicated, twice, that must be kept in step with `Config` forever; a
  new setting silently has no override until someone remembers the mirror. Lost
  to a table that both layers and the environment drive from the same entries.
- **Deserializing each layer into `Config` and diffing the result afterwards.**
  Cannot work: a default and an override are the same value by the time the
  struct is built.
- **Returning provenance beside the `Config` rather than inside it.** The
  Configuration screen asks a configuration where its values came from
  (`Config::provenance`), and a value that can be separated from its origin
  will be. Skipped serialization keeps it out of written documents, so the file
  format is unchanged.
- **Deriving variable names from keys at runtime.** One fewer line in the table,
  and in exchange no way to grep for a setting's variable and no place to write
  down an exception. The table is a list to be read, not computed.
- **JSON for list values in the environment** (`KTASK_TEST_GLOBS=["a","b"]`).
  Operators write globs, and quoting JSON inside a shell string is where that
  goes. Comma-splitting is what CI systems already mean by a list variable.
- **Treating an empty value as the empty string.** Contradicts ADR-0002, and no
  setting in the documented 22 has a meaning for `""`: the empty model name and
  the no-model case would become two spellings of one thing.
- **`Option<Source>` next to each field.** Provenance per field doubles the
  struct's field count and puts 22 keys into the written document that no reader
  can set.

## Consequences

- Every setting now has three spellings in one place — key, variable, field —
  and the tests that enumerate the documented keys fail until all three are
  added together, which is the point.
- A list entry cannot contain a comma when it is written in the environment.
  `test_globs` and `secret_patterns` can hold a comma in a document; use the
  file. Splitting is documented at the code that does it.
- A configuration path that points at a directory, or at a file the user cannot
  read, is an error rather than a shrug: a mistyped path fails at the first run
  instead of silently running on defaults.
- `Source::Flag` is declared and displayed but never produced by `load`: only an
  argument parser knows a flag was passed. The CLI needs a way to record a flag
  layer over a loaded `Config` — that entry point is a later task's, not this
  module's.
- `config.rs` is now a public module (`pub mod config`) so that `config::load`
  is reachable the way the tasks that call it spell it. Adding a key means one
  entry in `KEYS`; adding a *layer* means a new `Source` variant, a place in
  `load`'s order, and a provenance test, because the order is the contract.

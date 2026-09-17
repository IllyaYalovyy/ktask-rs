# 0015. The journal owns its filename, its schema, and its version row

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T015 gives `journal.rs` its first lines: `Journal`, `open`, `journal_path`,
`open_for`, the schema of `docs/DESIGN.md` Database schema, `journal_mode = WAL`,
`synchronous = FULL`, and a `schema_version` row in `meta`. Three questions come
with it, and none is answered by the document.

The first is who names the file. T014 already named it: ADR-0013 records that
registration writes its `repo_path` row into `<state_dir>/journal.db` through a
private `journal_database` helper and its own copy of the `meta` DDL, and says
that "`journal_path`/`Journal::open`" supersedes both. So the same filename and
the same `CREATE TABLE` text exist in two modules until one of them defers.

The second is what "opening a file from a future schema version is a clear error
rather than a corruption" costs. `docs/DESIGN.md` Error variants calls its list
"exact", so a new variant is a document change; `Error::Corrupt` is the variant a
program would reach for reflexively, and the task forbids it in the same breath as
the requirement.

The third is what the version row means for a file that has none. Registration
creates `meta` and one row and stops, so the database a project owns after
`ktask-rs init` is one the journal must be able to open rather than refuse.

Measured, not assumed:

- A fresh connection over a WAL database on this toolchain reports
  `PRAGMA synchronous = 2` (FULL) whether or not the connection asked for it:
  probed with the pragma dropped, and with `journal_mode = WAL` set on a
  connection that never mentioned `synchronous`. Only `journal_mode` is
  observable from the connection, which is what a test can assert.
- Read-only opening of a WAL journal works after the writing connection closes:
  probed by opening a journal, writing a row, dropping the handle (the `-wal` and
  `-shm` files are gone, leaving one file), and reading the schema and that row
  through `OpenFlags::SQLITE_OPEN_READ_ONLY`. This is what `project`'s
  `recorded_repository` does on every `discover`, so WAL had to be checked against
  it rather than assumed hostile.

## Decision

The journal module owns three facts and nothing else does. `journal_path` is the
only code that spells `journal.db`; the `meta` DDL exists once, as
`journal::CREATE_META_TABLE`, and `project` borrows it to write its row;
`Journal::open` creates every other object. `project.rs`'s private
`journal_database` helper and its copy of the DDL are deleted — ADR-0013 is the
note that said this would happen.

`Journal::open` reads before it writes. It asks the file for its `schema_version`
first, refuses a version above this build's, and only then states the pragmas,
runs the `IF NOT EXISTS` DDL, and stamps the row. A refusal therefore leaves a
newer file byte-for-byte in the shape its own tool wrote.

A journal from a later version is `Error::Config { key: "schema_version", .. }`.
Its variant documentation — "a configuration key was absent, malformed or carried
an unknown value" — describes it exactly: `schema_version` is a key, and it
carries a value this build cannot honour. ADR-0002 already treats this variant as
the place where the environment and the build disagree, which is what
`$XDG_STATE_HOME` pointing at an unusable directory is.

`meta` with no version row is version `0` rather than an error, because that is
what a registration leaves behind. Everything it lacks is created by the same DDL
a new file gets, so the upgrade is not a step of its own; the open stamps the row
to `SCHEMA_VERSION` with the value the file now has. A row that is present and is
not a number is `Error::Corrupt`: that file claims an age and cannot be read.

`Journal` holds its `Connection` privately and offers `schema_version()`, which
reads the row back from the file instead of echoing the constant the build was
compiled with.

Opening never creates a directory. A state directory that is absent is reported as
the refused open it is.

## Alternatives considered

- **Leave the filename and the `meta` DDL duplicated in `project.rs`.** Rejected:
  two spellings of one filename is how one repository ends up with two journals,
  and ADR-0013 already scheduled the replacement.
- **Have `register` call `Journal::open`.** Rejected for this task: registration
  writes one row, and making it create four tables, an index and a version row
  changes what T014's tests assert about the state directory's contents. The
  version-0 path is where the two meet instead, and a test proves that exact
  composition.
- **`Error::Corrupt` for a future version.** Rejected by the task's own
  done-when, and on the merits: the file is healthy, this program is the older
  one, and an operator told their journal is damaged goes looking for a backup
  rather than upgrading the tool.
- **`Error::Policy`, which can carry the path.** Rejected: in this codebase
  `Policy` means a repository or state-directory rule was broken by someone, and
  nobody broke anything by running a newer ktask-rs once.
- **A new `Error` variant (`SchemaTooNew`).** Rejected: `docs/DESIGN.md` fixes the
  variant list as exact and says later tasks depend on it by name. If the version
  refusal ever needs fields the `Config` pair cannot carry, that is the moment to
  reopen this, in an ADR that changes the document.
- **Refuse a file with no version row.** Rejected: it would make `init` and
  `Journal::open` disagree by construction, and absence carries the one answer
  that matters — the file predates versioning.
- **Stamp the version at creation only.** Rejected: writing the same value every
  open is what lets an interrupted or versionless file be carried forward by a
  re-run, and an idempotent write of an identical value changes no durable fact.
- **`journal_mode = WAL` with `synchronous = NORMAL`.** Rejected without
  discussion: `docs/DESIGN.md` names FULL because losing the last event is the
  failure the design exists to prevent. The pragma is stated even though it is
  this build's default, so a toolchain that changes the default cannot quietly
  change durability here.

## Consequences

`Journal::open` is the only writer of schema, so a later task adding a table adds
one statement to `SCHEMA` and raises `SCHEMA_VERSION` in one place. Raising it
without a step that brings older files forward is now a visible mistake: a version
below this one is stamped upward, so the migration step belongs beside
`stamp_version` the day a second version exists (VISION.md section 2 rules out
reading other tools' formats, not this tool outgrowing itself).

`journal.rs` and `project.rs` are now coupled in one direction only: `project`
knows the journal's path and the `meta` table, and the journal knows nothing about
registration. `repo_path` stays a string key shared between them by documentation,
which is cheap while there is one key; a second cross-module key is the point to
move the key names somewhere both modules can own.

Two things become harder to test, both named here. `synchronous = FULL` is asserted
as `2` and a mutant that deletes the pragma survives, because this SQLite reports
`2` to a connection that never asked — the assertion pins the observable contract,
the pragma pins the intent. And `Journal::open` refuses a missing state directory,
so every caller must have registered first; if a headless path ever needs a journal
before registration, the answer is a named helper that creates the directory at
`0700`, not a `create_dir_all` inside `open`.

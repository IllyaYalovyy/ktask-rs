# 0013. A registration is one row in the project's own journal

- **Status:** accepted
- **Date:** 2026-09-17

## Context

T014 asks `register` to create the state directory with mode 0700 "and writing
a `meta` row naming the repository path", and `discover` to find that project by
walking upward. `docs/DESIGN.md` fixes where the row lives — the `meta` table of
`<state_dir>/journal.db`, alongside `schema_version` — but nothing yet owns that
file: `journal.rs` with `Journal::open` and the full schema is T015, and the git
wrapper that could resolve a working copy is T044. So the row has to be written
by the code that needs it, and discovery has to read it back through the same
door.

What `discover` may conclude from what it finds is the actual decision. A state
directory is named by a hash of the repository's canonical path, so its *name*
already asserts which repository it belongs to; the row could be treated as a
detail and discovery could stop at `state_dir.is_dir()`. That is the version
where a directory created by an interrupted registration — or a hand-made one —
counts as a registration, and where the row nothing reads can rot without
anyone noticing. Three outcomes are possible for a directory that is there:
it names this repository, it names a different one, or it names nothing.

## Decision

Registration is the row. `discover` accepts an ancestor only when the directory
that ancestor's identity names holds a `meta` row whose value is that ancestor's
canonical path; the directory name alone is never enough.

The two refusals are different because their remedies are:

- **names nothing** (no journal, or a journal with no `repo_path` row) is
  `Error::Corrupt`. It is a registration that was interrupted between creating
  the directory and writing the row, and walking past it would let the next
  `init` overwrite evidence rather than finish what was started. `register`
  repairs exactly this case — absence is what it is allowed to write.
- **names another repository** is not this project's directory, so discovery
  passes it and keeps walking. `register` refuses it with `Error::Policy`
  instead of overwriting: one identity holding two paths is the durable-data
  ambiguity ADR-0011 refuses for event payloads, and here it would attribute one
  repository's journal to another.

`register` writes the row unconditionally (`ON CONFLICT DO UPDATE`), because a
re-registration that skips an existing row cannot repair the interrupted case,
and the value it writes for an existing row is the same value — an idempotent
call cannot change what durable data says.

Registration creates the `meta` table itself, with the DDL `docs/DESIGN.md`
gives for it, and nothing else. `journal.db` is named by one private
`journal_database` helper that T015's `journal_path` supersedes.

## Alternatives considered

- **Discovery trusts the directory's existence.** Rejected: it makes the row
  decorative, and the failure it buys is a false registration — a half-made
  directory answers `discover` as if the repository had been initialised.
- **A marker file in the state directory** (`registered`, `project.toml`).
  Rejected: it is a second source for one fact, and the schema already has a
  key/value table whose purpose is exactly this.
- **A `.ktask/` directory inside the repository** to discover from, as other
  supervisors do. Rejected on VISION.md section 11: no operational file of the
  supervisor's may live in the working copy, which is the reason discovery has to
  consult state outside it at all.
- **`Error::NotFound` for a state directory that records nothing.** Rejected:
  `NotFound` tells the operator to run `init` in a repository where they already
  did, and `init` then silently overwrites — the loud answer points at the file
  that is half-written.
- **Waiting for T015 and writing the row there.** Rejected: `discover` needs the
  row to work, and the task that ships registration must ship something that
  registers.

## Consequences

`ktask-core` touches SQLite from two modules until T015 lands, so the filename
and the `meta` DDL exist twice for one task cycle; T015 replaces both with
`journal_path`/`Journal::open` and this ADR is the note that says so. T015's
`CREATE TABLE IF NOT EXISTS` composes with the table registration creates, so no
migration is needed in either direction.

`register` refuses a root that does not exist and a root that is not a directory
rather than canonicalizing them away — the refusal ADR-0003 leaves to this
function, because `project_id` is infallible and cannot be the place that checks.

Permissions are set on every registration, not only at creation: a state
directory that somehow ended up group- or world-readable is tightened by the next
`init` rather than left in place, since the mode is a privacy guarantee
(VISION.md section 11) rather than a creation detail.

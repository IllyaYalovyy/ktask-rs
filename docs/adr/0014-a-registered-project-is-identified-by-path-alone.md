# 0014. A registered project is identified by path alone until the git wrapper lands

- **Status:** accepted
- **Date:** 2026-09-17

## Context

`docs/DESIGN.md` Paths gives `<project-id>` two halves: sixteen hex characters of
the canonical repository path, plus — "when one exists" — sixteen more for the
origin URL. `paths::project_id(repo_path, remote: Option<&str>)` takes both, so
`project::register` has to answer what to pass for `remote`.

The only honest source is `git config --get remote.origin.url`, and the crate has
no way to run it: `git::git`, the one typed subprocess helper, is T044, and
`git::remote_url` is T045. T014's file list is `project.rs` and `lib.rs`, and the
task says only that the id is computed "with `paths::project_id`". Meanwhile an id
is not cosmetic — it is the directory the journal lives in, and ADR-0003 makes it
load-bearing for recovery.

Two further constraints shape the choice. `register` is called from tests that may
neither set environment variables nor shell out to `git`, so whatever it passes for
the remote has to be stable in a scratch directory that is not a repository at
all. And the answer must be passed in exactly one place: a second call site that
later grows the origin half while the first does not would hand one repository two
identities, which is the failure ADR-0003 exists to prevent.

## Decision

`project::located` — the one function that turns a working copy into an identity —
passes `None`, and says so in its doc comment. Registration, discovery and the
directory name are therefore all computed from the canonical path alone,
consistently, from the first commit.

No `git` call is added here, and `.git/config` is not parsed by hand: a second
untyped subprocess, or a second parser for a format git owns, would be a
liability that outlives the task, and both belong to the helper `docs/DESIGN.md`
Dependencies already promises.

The remote half arrives in one commit that adds `git::remote_url` and changes the
single `None` at that call site.

## Alternatives considered

- **Shell out to `git` from `register`.** Rejected: it duplicates T044's helper in
  the one place a subprocess is least welcome — behind a function whose whole job
  is naming a directory — and would make `ktask-rs init` depend on a `git` binary
  and on the repository having an origin, neither of which `docs/DESIGN.md`
  requires of registration.
- **Read `.git/config` for `remote.origin.url`.** Rejected: git's config format has
  subsections, includes and `insteadOf` rewriting; a partial reader produces an id
  git itself would not have produced, and ADR-0003's whole argument is that one
  repository must not acquire two identities.
- **Add a `remote` parameter to `register`.** Rejected: the task fixes the
  signature, and a caller-supplied identity lets a mistyped remote split one
  repository across two journals. Identity is derived, never supplied.
- **Leave the id at the first half permanently.** Rejected: it contradicts
  `docs/DESIGN.md`, and the second half is exactly what distinguishes a moved or
  re-pointed clone, which ADR-0003 argues must not inherit the old journal.

## Consequences

Every id produced today is path-only, and will change when the origin half lands:
state directories registered before then are orphaned rather than renamed, because
the same repository under the new recipe names a different directory. ADR-0003
already records that migrating or aliasing registrations is a task nobody has
written; this ADR narrows that to a deadline — it must exist no later than the
task that turns the `None` into a lookup.

Registration of a directory that is not a git repository at all still works, which
is what the scratch fixtures in `project.rs` exercise and what the plan's ordering
requires: the git layer arrives later, and `init` should not acquire a dependency
on it early.

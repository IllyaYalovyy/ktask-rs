# 0003. Project identity hashes the canonical path's bytes

- **Status:** accepted
- **Date:** 2026-09-16

## Context

`<project-id>` names the directory one repository's journal lives in
(`docs/DESIGN.md` Paths), so it is load-bearing for recovery: two ids for one
repository means two journals that disagree about what was verified, and one id
for two repositories means events attributed to the wrong working copy.
`docs/DESIGN.md` fixes the recipe — first 16 hex of the SHA-256 of the canonical
repository path, plus `-` and the same for the origin URL when one exists — and
fixes `sha2` in the dependency set, leaving three things this task had to
decide.

`project_id` returns `String`, not `Result<String>`, and
`fs::canonicalize` fails: it requires the path to exist. So the recipe cannot
be followed literally for every input, and "hash the current directory joined
with the input" is not a way out either — the current directory is itself
something a caller can move, and an id that quietly depends on it splits one
repository per spelling.

Then: what bytes of the path get hashed, since a Unix file name is a byte
string rather than a string, and `Path`'s text spellings are lossy
(`to_string_lossy`) or platform-flavoured; and what an origin URL that arrives
as `""` means, since `git config --get` yields nothing at all when there is no
remote, while a layered config source can yield an empty value.

## Decision

Hash the bytes of the canonical path (`OsStr::as_encoded_bytes`, which is the
platform's own representation and never lossy), not a text rendering of it.
Where the filesystem cannot canonicalize, hash the path exactly as given; the
function stays infallible and the caller keeps one answer per spelling it was
handed. `ktask-rs register` resolves the working copy with git before asking,
so the paths that reach this in practice already exist.

An absent remote and an empty remote produce the same id: `Some("")` is
filtered out. The precedent is ADR-0002 and the same module's `value()`, which
treats an empty `$XDG_STATE_HOME` as unset — in both cases one repository must
not acquire two identities because a value arrived empty rather than missing.

`sha2` is adopted from the set `docs/DESIGN.md` fixes, with `workspace = true`;
`tempfile` is adopted as a dev-dependency for the same reason, so the symlink,
relative-path and non-UTF-8 fixtures live under the system temp directory as
`docs/DESIGN.md` Conventions requires. `Cargo.lock` moves in the same commit as
the dependency, because the build gate runs `--locked`.

## Alternatives considered

- **Return `Result<String>` and propagate the canonicalization error.**
  Rejected: `docs/DESIGN.md` fixes the signature, and every caller is a formatting
  path (naming a directory) with nothing to do with a failure but pass it along.
  It would also mean `register` could not print where state would go.
- **Fall back to `current_dir().join(path)`.** Rejected: it makes the id depend
  on a directory the caller never mentioned, so the same input can produce two
  ids across two invocations, which is the failure this function exists to
  prevent.
- **`path.display()` or `to_string_lossy()`.** Rejected: a file name that is not
  valid UTF-8 is legal on the platform this tool runs on, and a lossy rendering
  maps two such names to one id — the merge direction that silently mixes two
  journals.
- **Hash the absolute, lexically normalized path instead of canonicalizing.**
  Rejected: it leaves symlinks unresolved, and a repository reached through a
  symlink (a checkout under a versioned path, a mounted build directory) would
  get a second id.
- **Treat `Some("")` as a remote.** Rejected: it is the one case where an absent
  and an empty origin differ, and the difference is invisible until two state
  directories exist.
- **`blake3` or `crc32` for the digest.** Rejected: `sha2` is what
  `docs/DESIGN.md` fixes; a shorter digest buys nothing, and a non-SHA choice
  makes the values unreproducible with `sha256sum`, which is how the tests pin
  them.

## Consequences

16 hex characters is 64 bits: a collision between two repositories needs
birthday-work at that scale, and the id is a directory name rather than a
security decision, so the truncation the design specifies is safe here. If a
collision were ever observed it would surface as one project's state appearing
inside another's directory, which the `doctor` check is the place to report.

The id changes when either input changes: moving a working copy, or re-pointing
its origin, produces a new state directory rather than continuing the old
journal. That is intended — events recorded against a different working copy are
not evidence about this one — but it does mean a `git remote set-url` looks like
a new project, and migrating or aliasing registrations is a task no one has
written yet.

A path that does not exist has an id computed from its spelling, so a typo in a
registered path creates a state directory rather than being refused. That is the
price of an infallible `project_id`; `register` checking that the working copy
exists is where the refusal belongs.

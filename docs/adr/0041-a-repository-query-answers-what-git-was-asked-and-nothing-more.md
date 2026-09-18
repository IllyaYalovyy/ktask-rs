# 0041. A repository query answers what git was asked, and nothing more

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T045 adds the six read-only questions a run asks about a repository —
`head_sha`, `current_branch`, `remote_url`, `is_clean`, `status_porcelain`,
`fetch` — and ADR-0040 already fixed the transport under them: one argv, no
shell, trimmed stdout, `Error::Git` for every refusal. What it did not decide
is what each question *returns*, and four things make that a decision rather
than boilerplate.

**Detached HEAD is a normal state here, not an error.** VISION.md §10 has the
supervisor create each task worktree from a fetched SHA, and a checkout of a SHA
has no branch. So `current_branch` has three cases, not two: a name, no name,
and no commit to have a name about.

**The trim ADR-0040 chose is lossy for status.** Measured while writing this:
`git status --porcelain` prints a working-tree-only change as ` M seed.txt`, and
the first byte of that record is the staged column. The transport trims a
command's whole standard output, so the first record of a status answer arrives
as `M seed.txt` — the *staged* status. A wrapper that returned the records as
they came back would hand a caller a value that reads as the opposite of the
fact, and the case it corrupts (a single unstaged edit) is the common one.
Options checked: `-z` (NUL-separated) does not help — the trim still eats the
first record's leading space, and a rename becomes two records where the second
is a bare path, so a caller has to know git's wire format to count entries at
all. `--porcelain=v2` has no leading whitespace (records begin `1`, `2`, `?`),
but it moves the path after a tab and the status into its own field, which is a
different format to teach every later caller.

**A remote has no default in the documents.** VISION.md §10 says "remote
mainline" and names no remote. `docs/CONTRACT.md` adds nothing. So either the
wrapper invents `origin` or the caller names it.

**Output is how these get believed, and also how they get lied to.** A fetch
prints progress text that changes with git's version and terminal detection; a
status record's path is whatever git decided to print, which on this git (2.55)
includes C-quoting a filename holding a space. Parsing either into something
prettier means the supervisor's claim about a repository rests on this project's
parser rather than on git.

## Decision

Six wrappers over the one `git` call, each returning exactly what its command
answered — plus three shared rules.

- **A state a command answers with a sentinel is returned as `Option`.**
  `current_branch` runs `rev-parse --abbrev-ref HEAD` and maps git's literal
  `HEAD` to `None`. A branch cannot be named `HEAD`, so the sentinel is
  unambiguous, and the command still succeeded — reporting it as an error would
  make a detached worktree indistinguishable from a broken one. An *unborn*
  `HEAD` stays an error: "nothing has ever been committed" is a different fact
  from "commits exist and nothing is attached to one", and the taxonomy has to
  keep them apart.
- **Names the operator owns are never defaulted.** `remote_url` and `fetch` take
  the remote as an argument. Nothing in this module contains the string
  `origin`. A wrapper that guessed would publish somewhere nobody named, and
  VISION.md §10's evidence trail would still read as success.
- **Ask for the header, then drop it.** `status_porcelain` runs `status
  --porcelain --branch` and discards the `## …` line, which is always printed
  (clean, unborn, detached). That line exists so the trim lands on it: every
  record keeps both status columns. The header is then dropped, so no caller has
  to know it was ever there, and an unchanged repository answers with an empty
  list rather than a list holding one line.
- **Return git's own text, not a prettier version of it.** Records are git's
  lines, quoting included; `head_sha` is git's 40 hex characters;
  `remote_url` is `git remote get-url`'s answer, which is the URL git would
  actually use after any `insteadOf` rewrite. No wrapper re-formats an answer,
  so a claim about a repository is traceable to the command that formed it.
- **A fetch returns nothing.** `fetch -> Result<()>`: what it is *for* is the
  ref it moved, and that is read back with another git command rather than
  parsed off progress output. VISION.md §10 ends publication with a fetch whose
  whole purpose is to find out whether the remote moved; a cached or inferred
  answer there is the failure that turns an unpublished commit into a claim that
  it shipped.

## Alternatives considered

- **`status --porcelain` alone, records as git printed them** — one line shorter,
  and silently wrong for the single unstaged edit: the first record's staged
  column is whitespace and the transport trims whitespace. Rejected because the
  corruption is invisible in the common case and lands on exactly the distinction
  (staged versus not) the dirty-tree check needs.
- **`-z` and split on NUL** — the documented machine-readable form, and the usual
  answer to "paths contain odd bytes". It lost on both counts here: it does not
  survive the trim any better, and a rename emits two records for one change, so
  counting records counts the wrong thing.
- **`--porcelain=v2`** — no leading whitespace to lose, but it changes the format
  every later caller parses, for a problem one header line fixes.
- **`git symbolic-ref --short HEAD` for the branch** — its exit 1 *is* "detached",
  so returning `None` would mean deciding which non-zero exit to swallow, from
  stderr text, in a module whose rule is that every non-zero exit is
  `Error::Git`.
- **`git config --get remote.<name>.url`** — measured: exits 1 and prints
  nothing, so a missing remote is a failure with no explanation. `remote
  get-url` says `No such remote`, and answers the effective-URL question.
- **Defaulting the remote to `origin`** — the shortest caller, and a
  supervisor that publishes to a URL no configuration named.
- **Returning `String` from `fetch`, or a parsed summary** — output that varies
  with git's version and whether it thinks it has a terminal, for a fact better
  read from the ref the fetch moved.
- **Caching `head_sha` between calls** — publication's last step is *fetch, then
  compare*; a cache turns that into a comparison of a value with itself.

## Consequences

- Every read of repository state in a run is one line, and one line of git's
  output stands behind it: what a journal records and what an operator
  re-runs by hand are the same thing.
- The dirty-tree check (T048) gets records with both columns intact and an
  empty list for clean, so it can name which paths are modified, staged or
  untracked without asking git again. It inherits one real limitation, named
  here rather than discovered there: a quoted record is not a filename, so
  naming a path that git quoted means asking git one narrower question.
- `status_porcelain`'s correctness depends on `--branch` staying in the
  command. The coupling is pinned by two tests: one asserting a clean
  repository lists nothing (a leaked header reads as one change), one
  asserting a two-record dirty answer byte for byte (dropping or shifting the
  header loses a record).
- `is_clean` inherits git's defaults as the rule: ignored is not dirty,
  untracked is, staged-not-yet-committed is, and a wholly untracked directory
  collapses to one record naming it. A project that wants different defaults
  changes git's configuration, not this predicate — and says so where that
  configuration lives.
- Nothing here owns a time budget or an output cap, both inherited from
  ADR-0040. `fetch` is where those bite: a private remote with a credential
  prompt blocks the run, and `status` output is unbounded in a repository with a
  great many changes.

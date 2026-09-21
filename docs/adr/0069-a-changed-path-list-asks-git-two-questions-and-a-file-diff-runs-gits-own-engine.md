# 0069. A changed-path list asks git two questions, and a file diff runs git's own engine

- **Status:** accepted
- **Date:** 2026-09-20

## Context

Three callers need to know what a task worktree holds that its base does not,
and they need different things from the same repository. VISION.md §9 enforces a
phase's write scope — "the agent may add or modify tests only; production paths
are read-only" — which is only a rule if somebody reads a set of paths and
compares it, and ADR-0067 already holds up `git::changed_paths` as the evidence
the two path checks must share. §7 assembles a failure bundle with a "diff
summary" in it, inside a byte budget. §13's git screen shows one selected file's
diff to a human deciding whether the work is right.

Each of the three is one git command a human would also run. The difficulty is
that the obvious command — `git diff <base>` — is wrong for two of them, in ways
that are invisible until something acts on the answer:

- with rename detection on, `git diff --name-only` prints the *destination* of a
  rename and says nothing about the path that was deleted to make it;
- a file an agent wrote and never staged is in no commit, no index entry and so
  in no diff at all, which is precisely the file a write-scope check exists for;
- git quotes a path it finds unusual when it prints one for a terminal, so a
  name containing a `"` or a byte above 127 comes back as text that names no
  file once it is turned into a `PathBuf`;
- and `diff.external` / `textconv` let whatever a machine's configuration points
  at produce the diff a repository asks for — including the *contents* of a
  binary file, which §11 keeps out of logs and bundles.

Measured while writing this, on git 2.55: `git diff --name-status <base>` over a
staged rename printed `R100 old new`; `--name-only` printed `new` alone;
`--name-only --no-renames` printed both. `--stat` printed `Bin 5 -> 5 bytes` for
a binary file, and the same line with a `textconv` filter configured. A
configured filter turned that same file's diff into `cat`'s output.

## Decision

1. **`changed_paths` asks two questions and merges them.**
   `git diff --name-only --no-renames -z <base>` and
   `git ls-files --others --exclude-standard -z`, compared against the tree as it
   stands rather than against `HEAD`, and merged into one path-ordered set with
   each path in it once. A new file counts, because ADR-0044 already calls an
   untracked path uncommitted work and because a brand-new file is the likeliest
   way out of a scope; an *ignored* path does not, because `--exclude-standard` is
   the repository's own rule and a project that builds into its own tree would
   otherwise report its build output as the task's work. Both questions are asked
   inside this one function so that a caller cannot assemble half an answer.
2. **`--no-renames`, not rename parsing.** Detection off makes a rename the two
   paths it actually is — one deleted, one created — which is what a caller
   guarding a path has to see. It also means a machine's `diff.renames` cannot
   change the answer.
3. **`-z` on both listings, because the return type is a path.** The NUL-separated
   form switches git's quoting and octal escaping off and separates entries on a
   byte no path can hold, so splitting is exact. This is the opposite of
   `require_clean`, which keeps git's quoting on purpose (ADR-0041): its paths are
   half of a sentence a human reads, and these are paths somebody opens.
4. **`diff_summary` returns git's `--stat` verbatim**, tracked changes only, with
   nothing reformatted — widths, long-path abbreviation and the `Bin` line are
   git's decisions. Its two gaps (an untracked path is in no blob so it has no
   lines to count; a rename is one line, not two paths) are stated at both doors
   rather than papered over, because the door that needs completeness is
   `changed_paths`.
5. **`file_diff` runs git's own engine on a literal path.** `--no-ext-diff` and
   `--no-textconv`, with the path passed after a `--` as one `argv` entry. The two
   flags are used only here, because only here do they change anything: measured,
   `--stat` and `--name-only` are computed by git's diff engine and never reach an
   external driver or a filter, and a flag no test can contradict is a flag nobody
   can keep.

## Alternatives considered

- **`git status --porcelain` as the one source for `changed_paths`.** It does list
  untracked paths, and it is already parsed here. It lost twice: its comparison is
  the index against `HEAD`, not the tree against the `base` the task was built on,
  so a task that committed its own work reports nothing changed; and it collapses a
  wholly untracked directory into one name.
- **`--name-status -z`, pairing status fields with paths positionally.** The only
  way to get both halves of a rename out of one diff with detection on. It lost to
  `--no-renames` because the pairing has to handle a truncated record — a branch no
  real git output reaches, so no test reaches it either — and because it makes this
  module a parser of git's record grammar for a fact git will hand over freely if
  asked the other way.
- **Rename pairs as a richer return type.** T074 fixes the signature as
  `Result<Vec<PathBuf>>`, and a flat set is what both consumers do: §9 compares a
  path against a glob, ADR-0067 compares a path against a table.
- **`git diff --find-renames --raw -z`.** Same record-pairing problem.
- **libgit2 or gix instead of the `git` command.** Already settled by
  `docs/DESIGN.md` and ADR-0040; this module stays the one door.
- **Canonicalizing the paths before returning them.** Declined for the same reason
  ADR-0067 declined it: canonicalization answers symlink questions this module has
  no business answering and needs the worktree to answer them with.

## Consequences

- A write-scope check can be built on one call that sees a new file, a moved file's
  origin, a staged change and an unstaged one. Nothing consumes it yet: that is
  T091's `run_phase`, beside ADR-0067's check, so the two read one answer.
- `changed_paths` is the only door here that runs two git commands. It is not
  atomic — a tree edited between the two calls can be reported half one way and
  half the other — which is acceptable for a check whose verdict is "these paths
  were touched" and would not be for anything deciding a candidate's contents.
- A path that is not valid UTF-8 comes back with replacement characters, because
  this module decodes lossily module-wide (a deliberate existing trade: one stray
  byte must not stop a run). `--no-renames -z` reduces the damage — it is no longer
  *also* quoted and escaped — but fixing it properly means a byte-level transport,
  and that changes every door in this module rather than this one.
- A `path` outside the worktree reaches `Error::Git` carrying git's own "outside
  repository" sentence rather than `Error::Policy`. This module refuses with
  `Policy` where it enforces a rule of its own; here git enforced its own, and its
  words are also the evidence that the path was handed to git rather than opened
  by us.
- A path with no difference answers with an empty string, as does a path that
  exists in neither the base nor the tree. T150's pane shows nothing; that is the
  honest answer, and an error would be an invention.
